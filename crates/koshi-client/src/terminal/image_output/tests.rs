//! Tests for worker-side image eligibility, shared output state, and Sixel host modes.

use super::*;

use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;

use koshi_core::geometry::PixelCellSize;
use koshi_core::ids::PaneId;
use koshi_renderer::ImageCellState;
use koshi_terminal::graphics::{ImageAction, ImageDisplay};
use koshi_terminal::style::{Color, Style};
use koshi_test_support::output_workloads::SeededSequence;

impl ImageOutputState {
    /// Return the number of cells each latest placement covers on an output
    /// that draws over cells, in draw order.
    pub(crate) fn list_covered_cell_counts(&self) -> Vec<usize> {
        self.latest_encode_inputs
            .covered_cells
            .iter()
            .map(|covered_cells| covered_cells.len())
            .collect()
    }

    /// Return the bytes of decoded pixels the encode cache counts as held by
    /// the client's image records.
    pub(crate) fn get_held_image_byte_count(&self) -> u64 {
        self.encoded_output_cache.held_image_byte_count
    }

    /// Keep `compressed_bytes` as the zlib payload of the Kitty image
    /// `image_content_id` in the encode cache.
    pub(crate) fn keep_kitty_payload(
        &mut self,
        image_content_id: u64,
        compressed_bytes: Arc<[u8]>,
    ) {
        self.encoded_output_cache
            .insert_kitty_compressed_bytes(image_content_id, compressed_bytes);
    }

    /// Return the content id of each Kitty image whose zlib payload the
    /// encode cache keeps, in ascending order.
    pub(crate) fn list_kept_kitty_payload_content_ids(&self) -> Vec<u64> {
        let mut image_content_ids = self
            .encoded_output_cache
            .cached_output_entry_by_key
            .keys()
            .filter_map(|cached_output_key| match cached_output_key {
                CachedOutputKey::KittyCompressed { image_content_id } => Some(*image_content_id),
                CachedOutputKey::Units(_) => None,
            })
            .collect::<Vec<_>>();
        image_content_ids.sort_unstable();
        image_content_ids
    }

    /// Return the placements whose pixels the host shows, in draw order.
    pub(crate) fn list_shown_placement_keys(&self) -> Vec<ImagePlacementKey> {
        self.shown_placements
            .iter()
            .map(|shown_placement| shown_placement.placement_key)
            .collect()
    }

    /// Return whether a worker request is active or waits to start, or a
    /// result waits for the next placement selection.
    pub(crate) fn is_work_pending(&self) -> bool {
        self.active_job.is_some()
            || self.pending_worker_request.is_some()
            || self.has_unselected_results
    }
}

impl OutputPaint {
    /// Replace this paint's placement key with `placement_key`.
    fn set_placement_key(&mut self, placement_key: ImagePlacementKey) {
        self.placement_key = placement_key;
    }
}

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
    let mut image_transfer_state = None;
    let mut decoded_image = None;
    for template_unit in template_units {
        let iterm_command_bytes = template_unit
            .output_bytes
            .strip_prefix(b"\x1b]1337;")
            .expect("iTerm2 packet prefix")
            .strip_suffix(b"\x1b\\")
            .expect("iTerm2 packet terminator");
        if let Some(iterm_graphics) =
            koshi_iterm::parse_iterm_command(iterm_command_bytes, &mut image_transfer_state)
                .expect("generated iTerm2 packet parses")
        {
            decoded_image = Some(iterm_graphics.image);
        }
    }
    assert_eq!(image_transfer_state, None);
    decoded_image.expect("generated iTerm2 packets contain one image")
}

/// A request with frame generation `1` that encodes each of `output_paints`
/// for `output_kind` over `cell_snapshot`, under the encode inputs
/// [`build_frame_encode_inputs`] builds. The cell size is
/// `measured_pixel_cell_size` for Sixel and one pixel otherwise.
fn build_worker_request(
    output_kind: ImageOutputKind,
    cell_snapshot: Arc<ImageCellSnapshot>,
    output_paints: &[OutputPaint],
    measured_pixel_cell_size: Option<PixelCellSize>,
) -> WorkerRequest {
    let pixel_cell_size = if output_kind.is_sixel() {
        measured_pixel_cell_size.expect("Sixel test cell size")
    } else {
        PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell")
    };
    let frame_encode_inputs = build_frame_encode_inputs(
        output_kind,
        pixel_cell_size,
        measured_pixel_cell_size,
        Some(&cell_snapshot),
        output_paints,
    )
    .expect("valid test images");
    WorkerRequest {
        frame_generation: 1,
        output_kind,
        pixel_cell_size,
        measured_pixel_cell_size,
        cell_snapshot: Some(cell_snapshot),
        output_paints: output_paints.to_vec(),
        plan_outcomes: frame_encode_inputs.plan_outcomes,
        encode_keys: frame_encode_inputs
            .encode_keys
            .into_iter()
            .map(Some)
            .collect(),
        kitty_upload_requests: Vec::new(),
        is_cancellation_requested: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn oversized_sixel_tiles_cover_every_target_pixel_exactly_once() {
    let (image_pixel_width, image_pixel_height) = (256, 128);
    let mut seeded_sequence = SeededSequence::from_seed(0x1234_5678);
    let mut rgba_bytes = Vec::new();
    for _ in 0..image_pixel_width * image_pixel_height {
        let random_bits = seeded_sequence.compute_next_value();
        rgba_bytes.extend_from_slice(&[
            if random_bits & 1 == 0 { 0 } else { 255 },
            if random_bits & 2 == 0 { 0 } else { 255 },
            if random_bits & 4 == 0 { 0 } else { 255 },
            255,
        ]);
    }
    let output_paint =
        build_output_paint(rgba_bytes.clone(), image_pixel_width, image_pixel_height, 0);
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 256,
        maximum_pixel_width: Some(128),
        maximum_pixel_height: Some(128),
    };
    let pixel_cell_size = PixelCellSize::from_pixel_dimensions(1, 1).unwrap();
    let worker_request = build_worker_request(
        output_kind,
        Arc::new(build_blank_snapshot(output_paint.target_area)),
        std::slice::from_ref(&output_paint),
        Some(pixel_cell_size),
    );
    let template_units = encode_sixel_template(
        &worker_request,
        &Plan {
            output_paint: &output_paint,
            outcome: PlanOutcome {
                is_opaque: true,
                ..PlanOutcome::default()
            },
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
        rgba_bytes
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
                maximum_pixel_width: Some(1),
                maximum_pixel_height: Some(1),
            };
            let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).unwrap();
            let worker_request = build_worker_request(
                output_kind,
                Arc::new(build_blank_snapshot(output_paint.target_area)),
                std::slice::from_ref(&output_paint),
                Some(cell_size),
            );
            let template_units = encode_sixel_template(
                &worker_request,
                &Plan {
                    output_paint: &output_paint,
                    outcome: PlanOutcome {
                        is_opaque: true,
                        ..PlanOutcome::default()
                    },
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
                .map(|template_unit| {
                    (
                        template_unit.tile_offset,
                        decode_sixel_output_unit(&template_unit.output_bytes),
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
            maximum_pixel_width: None,
            maximum_pixel_height: None,
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
        let mut output_state = ImageOutputState::from_output_kind(None, Arc::new(|| {}));
        output_state.output_kind = Some(output_kind);
        let (worker_request_sender, worker_request_receiver) = mpsc::channel();
        output_state.worker_request_sender = Some(worker_request_sender);
        output_state.prepare_frame(
            &output_paints,
            Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 64, 64)))),
            Some(PixelCellSize::from_pixel_dimensions(1, 1).unwrap()),
        );
        let actual_output_paints = worker_request_receiver.try_recv().map(|worker_request| {
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

/// The content id of the next image [`build_output_paint`] builds.
static NEXT_IMAGE_CONTENT_ID: AtomicU64 = AtomicU64::new(1_000_000);

fn build_output_paint(
    rgba_bytes: Vec<u8>,
    image_pixel_width: u32,
    image_pixel_height: u32,
    z_index: i32,
) -> OutputPaint {
    let decoded_image = DecodedImage {
        pixel_width: image_pixel_width,
        pixel_height: image_pixel_height,
        rgba_bytes,
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
            z_index,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    });
    let placement_key = (PaneId::new(), 1);
    OutputPaint {
        placement_key,
        image_content_id: NEXT_IMAGE_CONTENT_ID.fetch_add(1, Ordering::Relaxed),
        image_record,
        target_area: Rect::new(0, 0, image_pixel_width as u16, image_pixel_height as u16),
        source_rect: image_source_rect,
        cell_pixel_offset_x: None,
        cell_pixel_offset_y: None,
        z_index,
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
    let image_plan = classify_output_paint(
        ImageOutputKind::Iterm,
        &image_cell_snapshot,
        &[],
        &output_paint,
    )
    .expect("the image remains in the output plan");
    assert_eq!(
        image_plan.outcome.image_compatibility,
        ImageCompatibility {
            has_text_layer_order_mismatch: true,
            ..ImageCompatibility::default()
        }
    );
}

#[test]
fn iterm_partial_alpha_on_a_default_blank_preserves_rgba() {
    let output_paint = build_output_paint(vec![10, 20, 30, 128], 1, 1, 1);
    let output_paints = [output_paint];
    let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1)));
    let image_plans =
        classify_frame_paints(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None)
            .expect("valid test images");

    assert_eq!(
        image_plans[0].outcome.image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        image_plans[0].outcome.iterm_composition,
        ItermComposition::default()
    );
    let worker_request = build_worker_request(
        ImageOutputKind::Iterm,
        Arc::clone(&cell_snapshot),
        &output_paints,
        None,
    );
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
    let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1)));
    let image_plans =
        classify_frame_paints(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None)
            .expect("valid test images");

    assert_eq!(
        image_plans[0].outcome.image_compatibility,
        ImageCompatibility::default()
    );
    let worker_request = build_worker_request(
        ImageOutputKind::Iterm,
        Arc::clone(&cell_snapshot),
        &output_paints,
        None,
    );
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
        classify_frame_paints(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None)
            .expect("valid test images");

    assert_eq!(
        image_plans[0].outcome.image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        image_plans[0].outcome.iterm_composition.background_color,
        Some([255, 0, 0])
    );
    let worker_request = build_worker_request(
        ImageOutputKind::Iterm,
        Arc::clone(&cell_snapshot),
        &output_paints,
        None,
    );
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
    let image_plans = classify_frame_paints(
        ImageOutputKind::Iterm,
        &cell_snapshot,
        &output_paints,
        Some(PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size")),
    )
    .expect("valid test images");

    assert_eq!(
        image_plans[0].outcome.image_compatibility,
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
        classify_frame_paints(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None)
            .expect("valid test images");

    assert_eq!(
        image_plans[0].outcome.image_compatibility,
        ImageCompatibility::default()
    );
    let worker_request = build_worker_request(
        ImageOutputKind::Iterm,
        Arc::new(cell_snapshot),
        &output_paints,
        None,
    );
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
    let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1)));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let image_plans = classify_frame_paints(
        ImageOutputKind::Iterm,
        &cell_snapshot,
        &output_paints,
        Some(cell_size),
    )
    .expect("valid test images");

    assert_eq!(
        image_plans[0].outcome.image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        image_plans[1].outcome.image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        image_plans[1].outcome.iterm_composition,
        ItermComposition {
            background_color: None,
            needs_per_cell_composition: true,
            has_known_per_cell_backgrounds: true,
        }
    );
    let worker_request = build_worker_request(
        ImageOutputKind::Iterm,
        Arc::clone(&cell_snapshot),
        &output_paints,
        Some(cell_size),
    );
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
fn the_worker_composes_a_partly_transparent_iterm_image_over_the_image_below_it() {
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let upper_output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let upper_placement_key = upper_output_paint.placement_key;
    let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1)));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let worker_request = build_worker_request(
        ImageOutputKind::Iterm,
        cell_snapshot,
        &[lower_output_paint, upper_output_paint],
        Some(cell_size),
    );
    let (worker_message_sender, worker_message_receiver) = mpsc::channel();

    run_worker_job(
        &worker_request,
        &worker_message_sender,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("the worker encodes");

    let upper_template_units = worker_message_receiver
        .try_iter()
        .filter_map(|worker_message| match worker_message {
            WorkerMessage::Unit(output_unit)
                if output_unit.placement_key == upper_placement_key =>
            {
                Some(TemplateUnit {
                    tile_offset: output_unit.tile_offset,
                    output_bytes: output_unit.output_bytes,
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        decode_iterm_output_units(&upper_template_units).rgba_bytes,
        [85, 0, 170, 192]
    );
}

#[test]
fn a_partly_transparent_iterm_image_over_another_image_keys_its_encode_to_the_measured_cell_size() {
    let pane_id = PaneId::new();
    let lower_image_paint = build_placed_image_paint(
        &build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
        (pane_id, 1),
        Rect::new(0, 0, 2, 2),
    );
    let upper_image_paint = build_placed_image_paint(
        &build_output_paint(vec![0, 0, 255, 128], 1, 1, 0),
        (pane_id, 2),
        Rect::new(1, 1, 2, 2),
    );
    let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
    let measured_pixel_cell_size = PixelCellSize::from_pixel_dimensions(2, 2);
    let (mut output_state, _worker_request_receiver, _worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Iterm);

    output_state.prepare_frame(
        &[lower_image_paint, upper_image_paint],
        cell_snapshot,
        measured_pixel_cell_size,
    );

    assert_eq!(
        output_state.latest_encode_inputs.encode_keys[0].composition_input,
        None
    );
    assert_eq!(
        output_state.latest_encode_inputs.encode_keys[1]
            .composition_input
            .as_ref()
            .map(|composition_input| composition_input.measured_pixel_cell_size),
        Some(measured_pixel_cell_size)
    );
}

#[test]
fn iterm_partial_lower_coverage_preserves_uncovered_default_pixels() {
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let mut upper_output_paint = build_output_paint(vec![0, 0, 255, 128, 0, 0, 0, 0], 2, 1, 1);
    upper_output_paint.target_area = Rect::new(0, 0, 2, 1);
    let output_paints = [lower_output_paint, upper_output_paint];
    let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 2, 1)));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let image_plans = classify_frame_paints(
        ImageOutputKind::Iterm,
        &cell_snapshot,
        &output_paints,
        Some(cell_size),
    )
    .expect("valid test images");
    let worker_request = build_worker_request(
        ImageOutputKind::Iterm,
        Arc::clone(&cell_snapshot),
        &output_paints,
        Some(cell_size),
    );
    let template_units = encode_iterm_template(
        &worker_request,
        &image_plans[1],
        &image_plans[..1],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("partly covered iTerm2 image encodes");

    assert_eq!(
        image_plans[1].outcome.image_compatibility,
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
    let image_plans = classify_frame_paints(
        ImageOutputKind::Iterm,
        &cell_snapshot,
        &output_paints,
        Some(cell_size),
    )
    .expect("valid test images");
    let worker_request = build_worker_request(
        ImageOutputKind::Iterm,
        Arc::clone(&cell_snapshot),
        &output_paints,
        Some(cell_size),
    );
    let template_units = encode_iterm_template(
        &worker_request,
        &image_plans[0],
        &[],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("per-cell iTerm2 image encodes");

    assert_eq!(
        image_plans[0].outcome.image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        decode_iterm_output_units(&template_units).rgba_bytes,
        [127, 0, 128, 255, 0, 0, 255, 128]
    );

    let image_plans_without_cell_size =
        classify_frame_paints(ImageOutputKind::Iterm, &cell_snapshot, &output_paints, None)
            .expect("valid test images");
    assert_eq!(
        image_plans_without_cell_size[0].outcome.image_compatibility,
        ImageCompatibility {
            has_iterm_alpha_mismatch: true,
            ..ImageCompatibility::default()
        }
    );
}

#[test]
fn kitty_background_layer_boundary_is_exact_for_native_host_protocols() {
    let explicit_background_cells = build_solid_cell_snapshot(Rect::new(0, 0, 1, 1), [1, 2, 3]);
    let default_background_cells = build_blank_snapshot(Rect::new(0, 0, 1, 1));
    for output_kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 2,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        },
    ] {
        let measured_cell_size = output_kind
            .is_sixel()
            .then(|| PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size"));
        let below_boundary_paints = [build_output_paint(vec![1, 2, 3, 255], 1, 1, -1_073_741_825)];
        let boundary_paints = [build_output_paint(vec![1, 2, 3, 255], 1, 1, -1_073_741_824)];

        assert_eq!(
            classify_frame_paints(
                output_kind,
                &explicit_background_cells,
                &below_boundary_paints,
                measured_cell_size,
            )
            .expect("valid test images")[0]
                .outcome
                .image_compatibility,
            ImageCompatibility {
                has_text_layer_order_mismatch: true,
                ..ImageCompatibility::default()
            },
            "{output_kind:?} below boundary over RGB"
        );
        assert_eq!(
            classify_frame_paints(
                output_kind,
                &explicit_background_cells,
                &boundary_paints,
                measured_cell_size,
            )
            .expect("valid test images")[0]
                .outcome
                .image_compatibility,
            ImageCompatibility::default(),
            "{output_kind:?} at boundary over RGB"
        );
        assert_eq!(
            classify_frame_paints(
                output_kind,
                &default_background_cells,
                &below_boundary_paints,
                measured_cell_size,
            )
            .expect("valid test images")[0]
                .outcome
                .image_compatibility,
            ImageCompatibility::default(),
            "{output_kind:?} below boundary over default background"
        );
        assert_eq!(
            classify_frame_paints(
                output_kind,
                &default_background_cells,
                &boundary_paints,
                measured_cell_size,
            )
            .expect("valid test images")[0]
                .outcome
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
    let mut blue_cell_style = Style::default();
    blue_cell_style.set_background_color(Color::Rgb(0, 0, 255));
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
                style: blue_cell_style,
                ..ImageCellState::default()
            },
        ],
    );
    let output_paint = build_output_paint(vec![255, 0, 0, 127, 255, 0, 0, 127], 2, 1, 0);
    let image_plan = classify_output_paint(
        ImageOutputKind::Iterm,
        &mixed_background_cells,
        &[],
        &output_paint,
    )
    .expect("the image remains in the output plan");
    assert_eq!(
        image_plan.outcome.iterm_composition,
        ItermComposition {
            background_color: None,
            needs_per_cell_composition: true,
            has_known_per_cell_backgrounds: true,
        }
    );
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
    let image_plan =
        classify_output_paint(ImageOutputKind::Iterm, &cell_snapshot, &[], &output_paint)
            .expect("the image remains in the output plan");
    assert_eq!(
        image_plan.outcome.iterm_composition,
        ItermComposition {
            background_color: None,
            needs_per_cell_composition: true,
            has_known_per_cell_backgrounds: true,
        }
    );
}

#[test]
fn sixel_partial_alpha_reencodes_when_the_cell_background_changes() {
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        maximum_pixel_width: None,
        maximum_pixel_height: None,
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
    let red_worker_request = build_worker_request(
        output_kind,
        Arc::clone(&red_cells),
        std::slice::from_ref(&output_paint),
        Some(cell_size),
    );
    let blue_worker_request = build_worker_request(
        output_kind,
        Arc::clone(&blue_cells),
        std::slice::from_ref(&output_paint),
        Some(cell_size),
    );
    assert_ne!(
        red_worker_request.encode_keys,
        blue_worker_request.encode_keys
    );

    let red_image_plan = classify_output_paint(output_kind, &red_cells, &[], &output_paint)
        .expect("red background is encodable");
    let blue_image_plan = classify_output_paint(output_kind, &blue_cells, &[], &output_paint)
        .expect("blue background is encodable");
    let red_template_units = encode_sixel_template(
        &red_worker_request,
        &red_image_plan,
        &[],
        2,
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("red output encodes");
    let blue_template_units = encode_sixel_template(
        &blue_worker_request,
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
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let upper_output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1)));
    let lower_image_plan =
        classify_output_paint(output_kind, &cell_snapshot, &[], &lower_output_paint)
            .expect("lower image is encodable");
    let upper_image_plan = classify_output_paint(
        output_kind,
        &cell_snapshot,
        std::slice::from_ref(&lower_output_paint),
        &upper_output_paint,
    )
    .expect("upper image remains in the output plan");
    let mut image_plans = vec![lower_image_plan, upper_image_plan];
    resolve_image_compatibility(output_kind, Some(cell_size), &mut image_plans);
    let upper_image_plan = &image_plans[1];
    assert_eq!(
        upper_image_plan.outcome.sixel_composition,
        SixelComposition {
            background_color: None,
            has_alpha: true,
            has_terminal_background: false,
        }
    );
    assert_eq!(
        upper_image_plan.outcome.image_compatibility,
        ImageCompatibility::default()
    );
    let worker_request = build_worker_request(
        output_kind,
        cell_snapshot,
        &[lower_output_paint.clone(), upper_output_paint.clone()],
        Some(cell_size),
    );
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
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let mut upper_output_paint = build_output_paint(vec![0, 0, 255, 0], 1, 1, 1);
    let mut upper_image_record = upper_output_paint.image_record.as_ref().clone();
    upper_image_record.display.sixel_background = Some(SixelBackground::Terminal);
    upper_output_paint.image_record = Arc::new(upper_image_record);
    let output_paints = [lower_output_paint, upper_output_paint];

    let default_cells = Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1)));
    let default_image_plans =
        classify_frame_paints(output_kind, &default_cells, &output_paints, Some(cell_size))
            .expect("valid test images");
    assert_eq!(
        default_image_plans[1].outcome.image_compatibility,
        ImageCompatibility {
            has_sixel_terminal_background_mismatch: true,
            ..ImageCompatibility::default()
        }
    );

    let rgb_cells = Arc::new(build_solid_cell_snapshot(
        Rect::new(0, 0, 1, 1),
        [0, 255, 0],
    ));
    let rgb_image_plans =
        classify_frame_paints(output_kind, &rgb_cells, &output_paints, Some(cell_size))
            .expect("valid test images");
    assert_eq!(
        rgb_image_plans[1].outcome.image_compatibility,
        ImageCompatibility::default()
    );
    let worker_request =
        build_worker_request(output_kind, rgb_cells, &output_paints, Some(cell_size));
    let template_units = encode_sixel_template(
        &worker_request,
        &rgb_image_plans[1],
        &rgb_image_plans[..1],
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
fn a_composed_sixel_pixel_with_zero_alpha_and_no_terminal_background_stays_transparent() {
    // The cell under the pixel is red, and the pixel shows nothing over it.
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let output_paint = build_output_paint(vec![10, 20, 30, 0], 1, 1, 0);
    let worker_request = build_worker_request(
        output_kind,
        Arc::new(build_solid_cell_snapshot(
            Rect::new(0, 0, 1, 1),
            [200, 0, 0],
        )),
        std::slice::from_ref(&output_paint),
        Some(cell_size),
    );
    let image_plan = Plan {
        output_paint: &output_paint,
        outcome: PlanOutcome::default(),
    };

    let composed_image = compose_sixel_tile(
        &worker_request,
        &image_plan,
        &[],
        TileRect {
            column_offset: 0,
            row_offset: 0,
            column_count: 1,
            row_count: 1,
        },
        Arc::clone(&output_paint.image_record.image),
    )
    .expect("the tile composes");

    assert_eq!(composed_image.rgba_bytes, [0, 0, 0, 0]);
}

#[test]
fn a_lower_sixel_pixel_with_zero_alpha_and_no_terminal_background_leaves_the_cell_color_below() {
    // The red cell `[200, 0, 0]` stays under the lower pixel, and the upper
    // pixel `[0, 0, 255]` at alpha 128 blends over it to `[100, 0, 128]`.
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let lower_output_paint = build_output_paint(vec![10, 20, 30, 0], 1, 1, 0);
    let upper_output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let worker_request = build_worker_request(
        output_kind,
        Arc::new(build_solid_cell_snapshot(
            Rect::new(0, 0, 1, 1),
            [200, 0, 0],
        )),
        &[lower_output_paint.clone(), upper_output_paint.clone()],
        Some(cell_size),
    );
    let lower_image_plan = Plan {
        output_paint: &lower_output_paint,
        outcome: PlanOutcome::default(),
    };
    let upper_image_plan = Plan {
        output_paint: &upper_output_paint,
        outcome: PlanOutcome::default(),
    };

    let composed_image = compose_sixel_tile(
        &worker_request,
        &upper_image_plan,
        &[lower_image_plan],
        TileRect {
            column_offset: 0,
            row_offset: 0,
            column_count: 1,
            row_count: 1,
        },
        Arc::clone(&upper_output_paint.image_record.image),
    )
    .expect("the tile composes");

    assert_eq!(composed_image.rgba_bytes, [100, 0, 128, 255]);
}

/// The encode inputs of `output_paints` for `output_kind` over
/// `cell_snapshot`, with a one-pixel cell size that the outer terminal
/// reported.
fn build_one_pixel_encode_inputs(
    output_kind: ImageOutputKind,
    cell_snapshot: &ImageCellSnapshot,
    output_paints: &[OutputPaint],
) -> FrameEncodeInputs {
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    build_frame_encode_inputs(
        output_kind,
        cell_size,
        Some(cell_size),
        Some(cell_snapshot),
        output_paints,
    )
    .expect("valid test images")
}

#[test]
fn a_sixel_encode_key_changes_with_the_cells_under_the_image_and_with_the_image() {
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };
    let layout_area = Rect::new(0, 0, 2, 1);
    let background_cell_snapshot = build_solid_cell_snapshot(layout_area, [20, 30, 40]);
    let background_cell_state = background_cell_snapshot
        .find_cell(0, 0)
        .expect("background cell")
        .clone();
    let glyph_cell_state = ImageCellState {
        character: 'X',
        ..background_cell_state.clone()
    };
    let glyph_under_image_cell_snapshot = build_image_cell_snapshot(
        layout_area,
        vec![glyph_cell_state.clone(), background_cell_state.clone()],
    );
    let glyph_beside_image_cell_snapshot =
        build_image_cell_snapshot(layout_area, vec![background_cell_state, glyph_cell_state]);
    let output_paint = build_output_paint(vec![255, 0, 0, 127], 1, 1, 0);
    let mut moved_output_paint = output_paint.clone();
    moved_output_paint.target_area.x = 1;
    let other_image_output_paint = build_output_paint(vec![255, 0, 0, 127], 1, 1, 0);
    let build_encode_key = |cell_snapshot: &ImageCellSnapshot, output_paint: &OutputPaint| {
        build_one_pixel_encode_inputs(
            output_kind,
            cell_snapshot,
            std::slice::from_ref(output_paint),
        )
        .encode_keys
        .remove(0)
    };
    let initial_encode_key = build_encode_key(&background_cell_snapshot, &output_paint);

    assert_eq!(
        build_encode_key(&background_cell_snapshot.clone(), &output_paint),
        initial_encode_key
    );
    assert_eq!(
        build_encode_key(&glyph_beside_image_cell_snapshot, &output_paint),
        initial_encode_key
    );
    assert_eq!(
        build_encode_key(&background_cell_snapshot, &moved_output_paint),
        initial_encode_key
    );
    assert_ne!(
        build_encode_key(&glyph_under_image_cell_snapshot, &output_paint),
        initial_encode_key
    );
    assert_ne!(
        build_encode_key(&glyph_beside_image_cell_snapshot, &moved_output_paint),
        initial_encode_key
    );
    assert_ne!(
        build_encode_key(&background_cell_snapshot, &other_image_output_paint),
        initial_encode_key
    );
}

#[test]
fn an_iterm_encode_key_changes_only_with_the_cells_under_its_own_image() {
    let output_kind = ImageOutputKind::Iterm;
    let layout_area = Rect::new(0, 0, 2, 1);
    let left_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let mut right_output_paint = build_output_paint(vec![0, 255, 0, 128], 1, 1, 0);
    right_output_paint.target_area.x = 1;
    let output_paints = [left_output_paint, right_output_paint];
    let mut blue_cell_style = Style::default();
    blue_cell_style.set_background_color(Color::Rgb(0, 0, 255));
    let blue_cell_state = ImageCellState {
        style: blue_cell_style,
        ..ImageCellState::default()
    };
    let initial_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &build_blank_snapshot(layout_area),
        &output_paints,
    );
    let right_change_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &build_image_cell_snapshot(
            layout_area,
            vec![ImageCellState::default(), blue_cell_state.clone()],
        ),
        &output_paints,
    );
    let left_change_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &build_image_cell_snapshot(layout_area, vec![blue_cell_state.clone(), blue_cell_state]),
        &output_paints,
    );

    assert_eq!(
        right_change_encode_inputs.encode_keys[0],
        initial_encode_inputs.encode_keys[0]
    );
    assert_ne!(
        right_change_encode_inputs.encode_keys[1],
        initial_encode_inputs.encode_keys[1]
    );
    assert_ne!(
        left_change_encode_inputs.encode_keys[0],
        right_change_encode_inputs.encode_keys[0]
    );
    assert_eq!(
        left_change_encode_inputs.encode_keys[1],
        right_change_encode_inputs.encode_keys[1]
    );
}

#[test]
fn an_iterm_encode_key_changes_with_the_image_below_it() {
    let output_kind = ImageOutputKind::Iterm;
    let cell_snapshot = build_blank_snapshot(Rect::new(0, 0, 1, 1));
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let other_lower_output_paint = build_output_paint(vec![0, 255, 0, 128], 1, 1, 0);
    let upper_output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let initial_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &cell_snapshot,
        &[lower_output_paint, upper_output_paint.clone()],
    );
    let changed_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &cell_snapshot,
        &[other_lower_output_paint, upper_output_paint],
    );

    assert_ne!(
        changed_encode_inputs.encode_keys[0],
        initial_encode_inputs.encode_keys[0]
    );
    assert_ne!(
        changed_encode_inputs.encode_keys[1],
        initial_encode_inputs.encode_keys[1]
    );
}

#[test]
fn an_iterm_encode_key_changes_with_cell_backgrounds_that_classify_alike() {
    let output_kind = ImageOutputKind::Iterm;
    let layout_area = Rect::new(0, 0, 2, 1);
    let output_paints = [build_output_paint([0, 0, 255, 128].repeat(2), 2, 1, 0)];
    let build_cell_state = |rgb_color: [u8; 3]| {
        let mut cell_style = Style::default();
        cell_style.set_background_color(Color::Rgb(rgb_color[0], rgb_color[1], rgb_color[2]));
        ImageCellState {
            style: cell_style,
            ..ImageCellState::default()
        }
    };
    let red_cell_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &build_image_cell_snapshot(
            layout_area,
            vec![build_cell_state([255, 0, 0]), ImageCellState::default()],
        ),
        &output_paints,
    );
    let green_cell_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &build_image_cell_snapshot(
            layout_area,
            vec![build_cell_state([0, 255, 0]), ImageCellState::default()],
        ),
        &output_paints,
    );

    assert_eq!(
        green_cell_encode_inputs.plan_outcomes,
        red_cell_encode_inputs.plan_outcomes
    );
    assert_eq!(
        red_cell_encode_inputs.plan_outcomes[0].iterm_composition,
        ItermComposition {
            background_color: None,
            needs_per_cell_composition: true,
            has_known_per_cell_backgrounds: true,
        }
    );
    assert_ne!(
        green_cell_encode_inputs.encode_keys,
        red_cell_encode_inputs.encode_keys
    );
}

#[test]
fn an_iterm_encode_key_changes_when_the_image_below_it_moves() {
    let output_kind = ImageOutputKind::Iterm;
    let cell_snapshot = build_blank_snapshot(Rect::new(0, 0, 3, 1));
    let lower_output_paint = build_output_paint([255, 0, 0, 128, 0, 255, 0, 128].to_vec(), 2, 1, 0);
    let mut moved_lower_output_paint = lower_output_paint.clone();
    moved_lower_output_paint.target_area.x = 1;
    let mut upper_output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    upper_output_paint.target_area.x = 1;
    let initial_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &cell_snapshot,
        &[lower_output_paint, upper_output_paint.clone()],
    );
    let moved_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &cell_snapshot,
        &[moved_lower_output_paint, upper_output_paint],
    );

    assert_eq!(
        moved_encode_inputs.plan_outcomes,
        initial_encode_inputs.plan_outcomes
    );
    assert_eq!(
        moved_encode_inputs.covered_cells[1],
        initial_encode_inputs.covered_cells[1]
    );
    assert_ne!(
        moved_encode_inputs.encode_keys[1],
        initial_encode_inputs.encode_keys[1]
    );
}

#[test]
fn an_iterm_encode_key_changes_when_the_image_below_it_draws_otherwise_over_cells_it_does_not_cover(
) {
    let output_kind = ImageOutputKind::Iterm;
    let layout_area = Rect::new(0, 0, 2, 1);
    let lower_output_paint = build_output_paint([255, 0, 0, 128].repeat(2), 2, 1, 0);
    let mut upper_output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    upper_output_paint.target_area.x = 1;
    let output_paints = [lower_output_paint, upper_output_paint];
    let initial_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &build_blank_snapshot(layout_area),
        &output_paints,
    );
    let glyph_beside_upper_image_encode_inputs = build_one_pixel_encode_inputs(
        output_kind,
        &build_image_cell_snapshot(
            layout_area,
            vec![
                ImageCellState {
                    character: 'X',
                    ..ImageCellState::default()
                },
                ImageCellState::default(),
            ],
        ),
        &output_paints,
    );

    assert_eq!(
        glyph_beside_upper_image_encode_inputs.covered_cells[1],
        initial_encode_inputs.covered_cells[1]
    );
    assert_eq!(
        initial_encode_inputs.plan_outcomes[0].image_compatibility,
        ImageCompatibility::default()
    );
    assert_eq!(
        glyph_beside_upper_image_encode_inputs.plan_outcomes[0].image_compatibility,
        ImageCompatibility {
            has_iterm_alpha_mismatch: true,
            ..ImageCompatibility::default()
        }
    );
    assert_ne!(
        glyph_beside_upper_image_encode_inputs.encode_keys[1],
        initial_encode_inputs.encode_keys[1]
    );
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
    let image_plan = classify_output_paint(
        ImageOutputKind::Sixel {
            palette_color_count: 2,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        },
        &cell_snapshot,
        &[],
        &output_paint,
    )
    .expect("transparent Sixel pixels remain in the output plan");
    assert_eq!(
        image_plan.outcome.image_compatibility,
        ImageCompatibility::default()
    );
}

#[test]
fn output_state_keeps_iterm_available_without_a_pixel_cell_query() {
    let output_state =
        ImageOutputState::from_output_kind(Some(ImageOutputKind::Iterm), Arc::new(|| {}));

    assert_eq!(output_state.get_output_kind(), Some(ImageOutputKind::Iterm));
    assert!(!output_state.is_work_pending());
}

#[test]
fn iterm_packet_accounting_uses_the_native_frame_limit() {
    assert_eq!(
        compute_checked_output_byte_count(
            MAX_SIXEL_OUTPUT_BYTE_COUNT,
            1,
            MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT
        )
        .expect("the native frame limit is larger"),
        MAX_SIXEL_OUTPUT_BYTE_COUNT + 1
    );
    assert!(matches!(
        compute_checked_output_byte_count(
            MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
            1,
            MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT
        ),
        Err(TemplateError::Failed)
    ));
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

    let mut raised_output_paint = second_output_paint;
    raised_output_paint.z_index = 1;
    assert_ne!(
        build_output_encode_key(ImageOutputKind::Iterm, cell_size, &first_output_paint),
        build_output_encode_key(ImageOutputKind::Iterm, cell_size, &raised_output_paint)
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

    let cropped_image =
        crop_output_image(&source_output_paint, None).expect("source rectangle is valid");

    assert_eq!(cropped_image.pixel_width, 1);
    assert_eq!(cropped_image.pixel_height, 1);
    assert_eq!(cropped_image.rgba_bytes, vec![0, 255, 0, 255]);
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

    let scaled_image = scale_output_tile(
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
    )
    .expect("the crop is inside the source image");

    assert_eq!(scaled_image.rgba_bytes, vec![0, 0, 255, 255]);
}

#[test]
fn worker_emits_one_complete_iterm_packet_for_one_opaque_pixel() {
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1)));
    let output_kind = ImageOutputKind::Iterm;
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let worker_request = build_worker_request(
        output_kind,
        cell_snapshot,
        std::slice::from_ref(&output_paint),
        Some(cell_size),
    );
    let (worker_message_sender, worker_message_receiver) = mpsc::channel();

    run_worker_job(
        &worker_request,
        &worker_message_sender,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("worker encodes the pixel");
    let worker_messages = worker_message_receiver.try_iter().collect::<Vec<_>>();

    let [WorkerMessage::Prepared {
        frame_generation: 1,
        placement_key: prepared_placement_key,
    }, WorkerMessage::Unit(output_unit)] = worker_messages.as_slice()
    else {
        panic!("the worker sends one Prepared message, then one packet: {worker_messages:?}");
    };
    assert_eq!(*prepared_placement_key, output_paint.placement_key);
    assert_eq!(
        (
            output_unit.frame_generation,
            output_unit.placement_key,
            output_unit.tile_offset
        ),
        (1, output_paint.placement_key, (0, 0))
    );
    assert_eq!(
        decode_iterm_output_units(&[TemplateUnit {
            tile_offset: output_unit.tile_offset,
            output_bytes: Arc::clone(&output_unit.output_bytes),
        }])
        .rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn worker_emits_a_complete_bounded_sixel_tile() {
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1)));
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let worker_request = build_worker_request(
        output_kind,
        cell_snapshot,
        std::slice::from_ref(&output_paint),
        Some(cell_size),
    );
    let (worker_message_sender, worker_message_receiver) = mpsc::channel();

    run_worker_job(
        &worker_request,
        &worker_message_sender,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("worker encodes the pixel");
    let worker_messages = worker_message_receiver.try_iter().collect::<Vec<_>>();

    let [WorkerMessage::Prepared {
        frame_generation: 1,
        placement_key: prepared_placement_key,
    }, WorkerMessage::Unit(output_unit)] = worker_messages.as_slice()
    else {
        panic!("the worker sends one Prepared message, then one tile: {worker_messages:?}");
    };
    assert_eq!(*prepared_placement_key, output_paint.placement_key);
    assert_eq!(
        (
            output_unit.frame_generation,
            output_unit.placement_key,
            output_unit.tile_offset
        ),
        (1, output_paint.placement_key, (0, 0))
    );
    assert!(output_unit.output_bytes.len() <= MAX_SIXEL_CHUNK_BYTE_COUNT);
    assert_eq!(
        decode_sixel_output_unit(&output_unit.output_bytes).rgba_bytes,
        [255, 0, 0, 255]
    );
}

#[test]
fn worker_emits_shared_templates_in_original_paint_order() {
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let pane_id = PaneId::new();
    for output_kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 2,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        },
    ] {
        let mut first_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        first_output_paint.set_placement_key((pane_id, 1));
        let mut middle_output_paint = build_output_paint(vec![0, 0, 255, 255], 1, 1, 0);
        middle_output_paint.set_placement_key((pane_id, 2));
        let mut last_output_paint = first_output_paint.clone();
        last_output_paint.set_placement_key((pane_id, 3));
        let output_paints = vec![first_output_paint, middle_output_paint, last_output_paint];
        let worker_request = build_worker_request(
            output_kind,
            Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1))),
            &output_paints,
            Some(cell_size),
        );
        let (worker_message_sender, worker_message_receiver) = mpsc::channel();

        run_worker_job(
            &worker_request,
            &worker_message_sender,
            MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
        )
        .expect("the worker encodes all paints");
        let output_units = worker_message_receiver
            .try_iter()
            .filter_map(|worker_message| match worker_message {
                WorkerMessage::Unit(output_unit) => Some(output_unit),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            output_units
                .iter()
                .map(|output_unit| output_unit.placement_key)
                .collect::<Vec<_>>(),
            [(pane_id, 1), (pane_id, 2), (pane_id, 3)],
            "{output_kind:?}"
        );
        let decoded_pixels = output_units
            .iter()
            .map(|output_unit| match output_kind {
                ImageOutputKind::Iterm => {
                    decode_iterm_output_units(&[TemplateUnit {
                        tile_offset: output_unit.tile_offset,
                        output_bytes: Arc::clone(&output_unit.output_bytes),
                    }])
                    .rgba_bytes
                }
                ImageOutputKind::Sixel { .. } => {
                    decode_sixel_output_unit(&output_unit.output_bytes).rgba_bytes
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
    let pane_id = PaneId::new();
    for output_kind in [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 2,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        },
    ] {
        for is_unavailable_paint_first in [true, false] {
            let mut unavailable_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
            unavailable_output_paint.set_placement_key((pane_id, 1));
            unavailable_output_paint.target_area.x = 1;
            let mut available_output_paint = build_output_paint(vec![0, 0, 255, 255], 1, 1, 0);
            available_output_paint.set_placement_key((pane_id, 2));
            let output_paints = if is_unavailable_paint_first {
                vec![unavailable_output_paint, available_output_paint]
            } else {
                vec![available_output_paint, unavailable_output_paint]
            };
            let worker_request = build_worker_request(
                output_kind,
                Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1))),
                &output_paints,
                Some(cell_size),
            );
            let (worker_message_sender, worker_message_receiver) = mpsc::channel();

            run_worker_job(
                &worker_request,
                &worker_message_sender,
                MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
            )
            .expect("the worker continues after an unavailable paint");
            let worker_messages = worker_message_receiver.try_iter().collect::<Vec<_>>();
            let unavailable_placement_keys = worker_messages
                .iter()
                .filter_map(|worker_message| match worker_message {
                    WorkerMessage::Unavailable { placement_key, .. } => Some(*placement_key),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let available_placement_keys = worker_messages
                .iter()
                .filter_map(|worker_message| match worker_message {
                    WorkerMessage::Unit(output_unit) => Some(output_unit.placement_key),
                    _ => None,
                })
                .collect::<Vec<_>>();

            assert_eq!(
                unavailable_placement_keys,
                [(pane_id, 1)],
                "{output_kind:?}"
            );
            assert_eq!(available_placement_keys, [(pane_id, 2)], "{output_kind:?}");
        }
    }
}

#[test]
fn worker_stops_before_unique_templates_exceed_the_frame_bound() {
    let output_kind = ImageOutputKind::Iterm;
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let pane_id = PaneId::new();
    let output_paints = (0u8..8)
        .map(|image_variant_index| {
            let mut output_paint = build_output_paint(
                vec![image_variant_index, 0, 255 - image_variant_index, 255],
                1,
                1,
                0,
            );
            output_paint.set_placement_key((pane_id, u64::from(image_variant_index) + 1));
            output_paint
        })
        .collect::<Vec<_>>();
    let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1)));
    let worker_request = build_worker_request(
        output_kind,
        Arc::clone(&cell_snapshot),
        &output_paints,
        Some(cell_size),
    );
    let image_plans = classify_frame_paints(
        output_kind,
        &cell_snapshot,
        &worker_request.output_paints,
        Some(cell_size),
    )
    .expect("valid test images");
    let first_template_byte_count = encode_iterm_template(
        &worker_request,
        &image_plans[0],
        &[],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("first template encodes")
    .iter()
    .map(|template_unit| template_unit.output_bytes.len())
    .sum::<usize>();
    let second_template_byte_count = encode_iterm_template(
        &worker_request,
        &image_plans[1],
        &image_plans[..1],
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("second template encodes")
    .iter()
    .map(|template_unit| template_unit.output_bytes.len())
    .sum::<usize>();
    let output_byte_limit = first_template_byte_count + second_template_byte_count - 1;
    let (worker_message_sender, worker_message_receiver) = mpsc::channel();

    assert_eq!(
        run_worker_job(&worker_request, &worker_message_sender, output_byte_limit),
        Err(())
    );
    let sent_message_summaries = worker_message_receiver
        .try_iter()
        .map(|worker_message| match worker_message {
            WorkerMessage::Prepared { placement_key, .. } => ("Prepared", placement_key),
            WorkerMessage::Unit(output_unit) => ("Unit", output_unit.placement_key),
            other_worker_message => panic!("unexpected worker message {other_worker_message:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        sent_message_summaries,
        [("Prepared", (pane_id, 1)), ("Unit", (pane_id, 1))]
    );
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
    let mut seeded_sequence = SeededSequence::from_seed(0x9e37_79b9);
    let mut rgba_bytes = Vec::with_capacity(image_pixel_width * image_pixel_height * 4);
    for _ in 0..image_pixel_width * image_pixel_height {
        rgba_bytes.extend_from_slice(&palette[seeded_sequence.compute_next_index(palette.len())]);
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
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };
    let cell_size =
        PixelCellSize::from_pixel_dimensions(image_pixel_width as u16, image_pixel_height as u16)
            .expect("test cell size");
    let worker_request = build_worker_request(
        output_kind,
        Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1))),
        std::slice::from_ref(&output_paint),
        Some(cell_size),
    );
    let template_units = encode_sixel_template(
        &worker_request,
        &Plan {
            output_paint: &output_paint,
            outcome: PlanOutcome {
                is_opaque: true,
                ..PlanOutcome::default()
            },
        },
        &[],
        palette.len(),
        None,
        None,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("one-cell Sixel output encodes");

    assert_eq!(template_units.len(), 1);
    assert!(template_units[0].output_bytes.len() > MAX_SIXEL_CHUNK_BYTE_COUNT);
    assert!(template_units[0].output_bytes.len() <= MAX_SIXEL_OUTPUT_BYTE_COUNT);
    assert_eq!(
        decode_sixel_output_unit(&template_units[0].output_bytes).rgba_bytes,
        rgba_bytes
    );
}

/// A selected placement of `output_paint` over its target area whose one unit
/// at offset `(0, 0)` holds `output_bytes`.
fn build_selected_unit_placement(
    output_paint: &OutputPaint,
    output_kind: ImageOutputKind,
    output_bytes: &[u8],
) -> SelectedPlacement {
    let pixel_cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell");
    SelectedPlacement {
        placement_key: output_paint.placement_key,
        target_area: output_paint.target_area,
        covered_cells: None,
        pixels: SelectedPixels::Units {
            encode_key: build_output_encode_key(output_kind, pixel_cell_size, output_paint),
            output_units: Arc::from(vec![TemplateUnit {
                tile_offset: (0, 0),
                output_bytes: Arc::from(output_bytes),
            }]),
        },
    }
}

#[test]
fn iterm_unit_output_has_exact_position_payload_and_cursor_restore() {
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let mut output_state = ImageOutputState::from_output_kind(None, Arc::new(|| {}));
    output_state.output_kind = Some(ImageOutputKind::Iterm);
    output_state.selected_placements = vec![build_selected_unit_placement(
        &output_paint,
        ImageOutputKind::Iterm,
        b"body",
    )];
    let frame_output_bytes = output_state
        .build_frame_output(Some(ratatui::layout::Position { x: 4, y: 5 }))
        .expect("frame output writes");
    assert_eq!(frame_output_bytes, b"\x1b[1;1Hbody\x1b[6;5H");
}

#[test]
fn sixel_unit_output_has_exact_mode_boundaries_and_cursor_restore() {
    let output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let output_kind = ImageOutputKind::Sixel {
        palette_color_count: 2,
        maximum_pixel_width: None,
        maximum_pixel_height: None,
    };
    let mut output_state = ImageOutputState::from_output_kind(None, Arc::new(|| {}));
    output_state.output_kind = Some(output_kind);
    output_state.selected_placements = vec![build_selected_unit_placement(
        &output_paint,
        output_kind,
        b"sixel",
    )];
    let frame_output_bytes = output_state
        .build_frame_output(Some(ratatui::layout::Position { x: 4, y: 5 }))
        .expect("frame output writes");
    assert_eq!(
        frame_output_bytes,
        b"\x1b[?80l\x1b[?8452l\x1b[?1070h\x1b[1;1Hsixel\x1b[6;5H\x1b[?80r\x1b[?8452r\x1b[?1070r"
    );
}

fn build_blank_snapshot(layout_area: Rect) -> ImageCellSnapshot {
    ImageCellSnapshot::from_cell_states(
        layout_area,
        vec![
            ImageCellState::default();
            usize::from(layout_area.width) * usize::from(layout_area.height)
        ],
    )
    .expect("test cells fit area")
}

/// An output state for `output_kind` whose worker requests go to the returned
/// receiver and whose worker messages come from the returned sender. Kitty
/// image ids start at `1`.
fn build_output_state_with_worker_channels(
    output_kind: ImageOutputKind,
) -> (
    ImageOutputState,
    mpsc::Receiver<WorkerRequest>,
    mpsc::Sender<WorkerMessage>,
) {
    let mut output_state = ImageOutputState::from_output_kind(None, Arc::new(|| {}));
    output_state.output_kind = Some(output_kind);
    output_state.next_kitty_image_id = 1;
    let (worker_request_sender, worker_request_receiver) = mpsc::channel();
    output_state.worker_request_sender = Some(worker_request_sender);
    let (worker_message_sender, worker_messages) = mpsc::channel();
    output_state.worker_messages = Some(worker_messages);
    (output_state, worker_request_receiver, worker_message_sender)
}

/// Send, for each paint the iTerm2 or Sixel request `worker_request` encodes,
/// a `Prepared` message and one unit at offset `(0, 0)` whose bytes are
/// `unit <generation>:<placement id>`, then the request's `Finished` message.
fn finish_request_with_units(
    worker_message_sender: &mpsc::Sender<WorkerMessage>,
    worker_request: &WorkerRequest,
) {
    for (placement_key, _) in worker_request.list_requested_encode_keys() {
        worker_message_sender
            .send(WorkerMessage::Prepared {
                frame_generation: worker_request.frame_generation,
                placement_key,
            })
            .expect("the state receives");
        worker_message_sender
            .send(WorkerMessage::Unit(OutputUnit {
                frame_generation: worker_request.frame_generation,
                placement_key,
                tile_offset: (0, 0),
                output_bytes: Arc::from(
                    format!(
                        "unit {}:{}",
                        worker_request.frame_generation, placement_key.1
                    )
                    .into_bytes(),
                ),
            }))
            .expect("the state receives");
    }
    finish_request(worker_message_sender, worker_request, false);
}

/// Send one upload per image of the Kitty request `worker_request`, with the
/// bytes `upload <id>;` and the zlib payload the request carries for the
/// image, or `zlib <content id>` when it carries none, then the request's
/// `Finished` message.
fn finish_request_with_kitty_uploads(
    worker_message_sender: &mpsc::Sender<WorkerMessage>,
    worker_request: &WorkerRequest,
) {
    for kitty_upload_request in &worker_request.kitty_upload_requests {
        worker_message_sender
            .send(WorkerMessage::KittyUploaded {
                frame_generation: worker_request.frame_generation,
                finished_kitty_upload: FinishedKittyUpload {
                    image_content_id: kitty_upload_request.image_content_id,
                    kitty_image_id: kitty_upload_request.kitty_image_id,
                    upload_bytes: Arc::from(
                        format!("upload {};", kitty_upload_request.kitty_image_id).into_bytes(),
                    ),
                },
                compressed_bytes: kitty_upload_request
                    .compressed_bytes
                    .clone()
                    .unwrap_or_else(|| {
                        Arc::from(
                            format!("zlib {}", kitty_upload_request.image_content_id).into_bytes(),
                        )
                    }),
            })
            .expect("the state receives");
    }
    finish_request(worker_message_sender, worker_request, false);
}

/// Send the `Finished` message of `worker_request` with `has_failed`.
fn finish_request(
    worker_message_sender: &mpsc::Sender<WorkerMessage>,
    worker_request: &WorkerRequest,
    has_failed: bool,
) {
    worker_message_sender
        .send(WorkerMessage::Finished {
            frame_generation: worker_request.frame_generation,
            has_failed,
        })
        .expect("the state receives");
}

/// Write what one paint writes after `prepare_frame`: the frame reset, and,
/// when the selected pixels differ from the host's, the frame output with no
/// cursor, after which the frame commits. Returns the reset bytes and the
/// output bytes as text.
fn paint_prepared_frame(output_state: &mut ImageOutputState) -> (String, String) {
    let is_native_commit = output_state.is_native_commit_pending();
    let frame_output_bytes = if is_native_commit {
        output_state
            .build_frame_output(None)
            .expect("the frame output writes")
    } else {
        Vec::new()
    };
    let mut frame_reset_bytes = Vec::new();
    output_state
        .write_frame_reset(&mut frame_reset_bytes)
        .expect("the frame reset writes");
    if is_native_commit {
        output_state.commit_frame();
    }
    (
        String::from_utf8(frame_reset_bytes).expect("reset bytes are text"),
        String::from_utf8(frame_output_bytes).expect("output bytes are text"),
    )
}

/// A paint of `source_output_paint`'s image, under its content id, as the
/// placement `placement_key` over `target_area`, with its source rectangle
/// and z-index `0`.
fn build_placed_image_paint(
    source_output_paint: &OutputPaint,
    placement_key: ImagePlacementKey,
    target_area: Rect,
) -> ImagePaint {
    ImagePaint::from_image_placement(
        placement_key.0,
        placement_key.1,
        source_output_paint.image_content_id,
        Arc::clone(&source_output_paint.image_record),
        target_area,
        source_output_paint.source_rect,
        0,
    )
}

/// The iTerm2 and Sixel output kinds the state tests run under.
fn list_cell_output_kinds() -> [ImageOutputKind; 2] {
    [
        ImageOutputKind::Iterm,
        ImageOutputKind::Sixel {
            palette_color_count: 256,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        },
    ]
}

/// The frame output one unit with `unit_text` writes at row `row_index`,
/// column 0, with no cursor, under `output_kind`.
fn build_unit_frame_output(
    output_kind: ImageOutputKind,
    row_index: u16,
    unit_text: &str,
) -> String {
    let cursor_text = format!("\x1b[{};1H", row_index + 1);
    match output_kind {
        ImageOutputKind::Sixel { .. } => format!(
            "\x1b[?80l\x1b[?8452l\x1b[?1070h{cursor_text}{unit_text}\x1b[?25l\x1b[?80r\x1b[?8452r\x1b[?1070r"
        ),
        ImageOutputKind::Iterm | ImageOutputKind::Kitty => {
            format!("{cursor_text}{unit_text}\x1b[?25l")
        }
    }
}

#[test]
fn a_frame_shows_no_pixels_while_its_encode_runs_and_its_units_once_it_ends() {
    for output_kind in list_cell_output_kinds() {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        );
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);

        output_state.prepare_frame(
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        let worker_request = worker_request_receiver
            .try_recv()
            .expect("the frame reaches the worker");
        assert_eq!(
            paint_prepared_frame(&mut output_state),
            (String::new(), String::new()),
            "{output_kind:?}"
        );
        assert_eq!(output_state.list_unavailable_placement_keys(), []);

        finish_request_with_units(&worker_message_sender, &worker_request);
        output_state.prepare_frame(std::slice::from_ref(&image_paint), cell_snapshot, cell_size);
        assert_eq!(
            paint_prepared_frame(&mut output_state),
            (
                String::new(),
                build_unit_frame_output(output_kind, 0, "unit 1:1")
            ),
            "{output_kind:?}"
        );
        assert!(!output_state.is_work_pending(), "{output_kind:?}");
    }
}

#[test]
fn a_newer_frame_lets_the_active_encode_end_and_replaces_only_the_waiting_request() {
    for output_kind in list_cell_output_kinds() {
        let placement_key = (PaneId::new(), 1);
        let image_paints = [
            build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
            build_output_paint(vec![0, 255, 0, 255], 1, 1, 0),
            build_output_paint(vec![0, 0, 255, 255], 1, 1, 0),
        ]
        .map(|source_output_paint| {
            build_placed_image_paint(&source_output_paint, placement_key, Rect::new(0, 0, 1, 1))
        });
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);

        output_state.prepare_frame(&image_paints[0..1], cell_snapshot.clone(), cell_size);
        let first_worker_request = worker_request_receiver
            .try_recv()
            .expect("the first frame reaches the worker");
        output_state.prepare_frame(&image_paints[1..2], cell_snapshot.clone(), cell_size);
        let second_cancellation_token = Arc::clone(
            &output_state
                .pending_worker_request
                .as_ref()
                .expect("the second frame waits")
                .is_cancellation_requested,
        );
        output_state.prepare_frame(&image_paints[2..3], cell_snapshot.clone(), cell_size);

        assert!(
            !first_worker_request
                .is_cancellation_requested
                .load(Ordering::Acquire),
            "{output_kind:?} cancelled the active encode"
        );
        assert!(
            second_cancellation_token.load(Ordering::Acquire),
            "{output_kind:?} kept the replaced request"
        );
        assert_eq!(
            worker_request_receiver.try_recv().err(),
            Some(TryRecvError::Empty),
            "{output_kind:?}"
        );

        finish_request_with_units(&worker_message_sender, &first_worker_request);
        output_state.prepare_frame(&image_paints[2..3], cell_snapshot.clone(), cell_size);
        let third_worker_request = worker_request_receiver
            .try_recv()
            .expect("the newest frame starts after the first");
        assert!(
            Arc::ptr_eq(
                &third_worker_request.output_paints[0].image_record.image,
                &image_paints[2].image_record.image
            ),
            "{output_kind:?} started another frame than the newest"
        );
        assert_eq!(
            paint_prepared_frame(&mut output_state),
            (
                String::new(),
                build_unit_frame_output(output_kind, 0, "unit 1:1")
            ),
            "{output_kind:?} did not show the older pixels"
        );

        finish_request_with_units(&worker_message_sender, &third_worker_request);
        output_state.prepare_frame(&image_paints[2..3], cell_snapshot, cell_size);
        let third_unit_text = format!("unit {}:1", third_worker_request.frame_generation);
        assert_eq!(
            paint_prepared_frame(&mut output_state),
            (
                String::from("\x1b[2J"),
                build_unit_frame_output(output_kind, 0, &third_unit_text)
            ),
            "{output_kind:?}"
        );
    }
}

#[test]
fn older_pixels_of_another_size_are_not_shown_while_the_new_size_encodes() {
    for output_kind in list_cell_output_kinds() {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let small_image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        );
        let large_image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 2, 2),
        );
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        encode_and_paint_frame(
            &mut output_state,
            &worker_request_receiver,
            &worker_message_sender,
            std::slice::from_ref(&small_image_paint),
            cell_snapshot.clone(),
            cell_size,
        );

        output_state.prepare_frame(
            std::slice::from_ref(&large_image_paint),
            cell_snapshot,
            cell_size,
        );

        assert_eq!(
            worker_request_receiver
                .try_recv()
                .map(|worker_request| list_requested_placement_keys(&worker_request)),
            Ok(vec![source_output_paint.placement_key]),
            "{output_kind:?} did not encode the new size"
        );
        assert_eq!(
            paint_prepared_frame(&mut output_state),
            (String::from("\x1b[2J"), String::new()),
            "{output_kind:?}"
        );
    }
}

#[test]
fn a_failed_encode_shows_the_unavailable_text_until_the_image_size_changes() {
    for output_kind in list_cell_output_kinds() {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        );
        let resized_image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 2, 2),
        );
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);

        output_state.prepare_frame(
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        let worker_request = worker_request_receiver
            .try_recv()
            .expect("the frame reaches the worker");
        finish_request(&worker_message_sender, &worker_request, true);
        output_state.prepare_frame(
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        assert_eq!(
            output_state.list_unavailable_placement_keys(),
            [source_output_paint.placement_key],
            "{output_kind:?}"
        );
        output_state.prepare_frame(
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        assert_eq!(
            worker_request_receiver.try_recv().err(),
            Some(TryRecvError::Empty),
            "{output_kind:?} encoded the failed frame again"
        );
        assert!(!output_state.is_work_pending(), "{output_kind:?}");

        output_state.prepare_frame(
            std::slice::from_ref(&resized_image_paint),
            cell_snapshot,
            cell_size,
        );
        assert_eq!(
            worker_request_receiver
                .try_recv()
                .map(|worker_request| list_requested_placement_keys(&worker_request)),
            Ok(vec![source_output_paint.placement_key]),
            "{output_kind:?} did not encode the resized image"
        );
        assert_eq!(output_state.list_unavailable_placement_keys(), []);
    }
}

#[test]
fn a_placement_whose_units_pass_the_frame_limit_shows_the_unavailable_text() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let image_paints = [
        build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        ),
        build_placed_image_paint(
            &source_output_paint,
            (source_output_paint.placement_key.0, 2),
            Rect::new(0, 2, 1, 1),
        ),
    ];
    let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
    let (mut output_state, worker_request_receiver, _worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Iterm);
    output_state.prepare_frame(&image_paints, cell_snapshot.clone(), cell_size);
    worker_request_receiver
        .try_recv()
        .expect("the frame reaches the worker");
    let largest_fitting_unit_bytes: Arc<[u8]> = Arc::from(vec![
        b'x';
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT
            - FRAME_FRAMING_BYTE_COUNT
            - UNIT_FRAMING_BYTE_COUNT
    ]);
    let one_byte_unit_bytes: Arc<[u8]> = Arc::from(vec![b'x']);
    for (paint_index, (image_paint, output_bytes)) in image_paints
        .iter()
        .zip([largest_fitting_unit_bytes, one_byte_unit_bytes])
        .enumerate()
    {
        let placement_key = (image_paint.pane_id, image_paint.placement_id);
        output_state.encoded_placement_by_key.insert(
            placement_key,
            EncodedPlacement {
                encode_key: output_state.latest_encode_inputs.encode_keys[paint_index].clone(),
                encoded_pixels: EncodedPixels::Drawn(Arc::from(vec![TemplateUnit {
                    tile_offset: (0, 0),
                    output_bytes,
                }])),
            },
        );
    }
    output_state.has_unselected_results = true;

    output_state.prepare_frame(&image_paints, cell_snapshot, cell_size);

    assert_eq!(
        output_state
            .selected_placements
            .iter()
            .map(|selected_placement| selected_placement.placement_key)
            .collect::<Vec<_>>(),
        [(image_paints[0].pane_id, image_paints[0].placement_id)]
    );
    assert_eq!(
        output_state.list_unavailable_placement_keys(),
        [(image_paints[1].pane_id, image_paints[1].placement_id)]
    );
}

#[test]
fn disconnected_worker_settles_each_distinct_frame_as_unavailable() {
    let mut output_state = ImageOutputState::from_output_kind(None, Arc::new(|| {}));
    output_state.output_kind = Some(ImageOutputKind::Iterm);
    let (worker_request_sender, worker_request_receiver) = mpsc::channel();
    drop(worker_request_receiver);
    output_state.worker_request_sender = Some(worker_request_sender);
    let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 1, 1))));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
    let first_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let first_image_paint = build_placed_image_paint(
        &first_output_paint,
        first_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    );
    let second_output_paint = build_output_paint(vec![0, 0, 255, 255], 1, 1, 0);
    let second_image_paint = build_placed_image_paint(
        &second_output_paint,
        (second_output_paint.placement_key.0, 2),
        Rect::new(0, 0, 1, 1),
    );

    output_state.prepare_frame(
        std::slice::from_ref(&first_image_paint),
        cell_snapshot.clone(),
        cell_size,
    );
    assert_eq!(
        output_state.list_unavailable_placement_keys(),
        [(first_image_paint.pane_id, first_image_paint.placement_id)]
    );
    assert!(!output_state.is_work_pending());
    paint_prepared_frame(&mut output_state);

    output_state.prepare_frame(
        std::slice::from_ref(&second_image_paint),
        cell_snapshot,
        cell_size,
    );
    assert_eq!(
        output_state.list_unavailable_placement_keys(),
        [(second_image_paint.pane_id, second_image_paint.placement_id)]
    );
    assert!(!output_state.is_work_pending());
}

#[test]
fn worker_output_past_the_frame_limit_fails_the_request_and_cancels_it() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let image_paint = build_placed_image_paint(
        &source_output_paint,
        source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    );
    let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Iterm);
    output_state.prepare_frame(
        std::slice::from_ref(&image_paint),
        cell_snapshot.clone(),
        cell_size,
    );
    let worker_request = worker_request_receiver
        .try_recv()
        .expect("the frame reaches the worker");
    output_state
        .active_job
        .as_mut()
        .expect("the request runs")
        .received_byte_count = MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT;

    finish_request_with_units(&worker_message_sender, &worker_request);
    output_state.prepare_frame(std::slice::from_ref(&image_paint), cell_snapshot, cell_size);

    assert!(worker_request
        .is_cancellation_requested
        .load(Ordering::Acquire));
    assert_eq!(
        output_state.list_unavailable_placement_keys(),
        [(image_paint.pane_id, image_paint.placement_id)]
    );
    assert!(!output_state.is_work_pending());
}

#[test]
fn a_cancelled_request_drops_its_results_and_starts_the_waiting_one() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let image_paint = build_placed_image_paint(
        &source_output_paint,
        source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    );
    let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Iterm);
    output_state.prepare_frame(
        std::slice::from_ref(&image_paint),
        cell_snapshot.clone(),
        cell_size,
    );
    let first_worker_request = worker_request_receiver
        .try_recv()
        .expect("the first frame reaches the worker");

    output_state.reset_connection();
    output_state.prepare_frame(
        std::slice::from_ref(&image_paint),
        cell_snapshot.clone(),
        cell_size,
    );
    assert!(first_worker_request
        .is_cancellation_requested
        .load(Ordering::Acquire));
    assert_eq!(
        worker_request_receiver.try_recv().err(),
        Some(TryRecvError::Empty)
    );
    finish_request_with_units(&worker_message_sender, &first_worker_request);
    output_state.prepare_frame(std::slice::from_ref(&image_paint), cell_snapshot, cell_size);

    assert!(output_state.encoded_placement_by_key.is_empty());
    assert_eq!(output_state.selected_placements, []);
    assert_eq!(
        worker_request_receiver
            .try_recv()
            .map(|worker_request| list_requested_placement_keys(&worker_request)),
        Ok(vec![source_output_paint.placement_key]),
        "the frame after the reset did not start"
    );
}

#[test]
fn an_opaque_iterm_or_sixel_image_that_moves_reuses_its_encoded_output() {
    for output_kind in list_cell_output_kinds() {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell");
        let cell_snapshot = Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8)));
        let (image_output_ready_sender, image_output_ready_receiver) = mpsc::channel();
        let mut output_state = ImageOutputState::from_output_kind(
            Some(output_kind),
            Arc::new(move || {
                let _ = image_output_ready_sender.send(());
            }),
        );

        loop {
            output_state.prepare_frame(
                &[build_placed_image_paint(
                    &source_output_paint,
                    source_output_paint.placement_key,
                    Rect::new(0, 0, 1, 1),
                )],
                Some(Arc::clone(&cell_snapshot)),
                Some(cell_size),
            );
            if !output_state.is_work_pending() {
                break;
            }
            image_output_ready_receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{output_kind:?} did not settle"));
        }
        let (_, first_frame_output_text) = paint_prepared_frame(&mut output_state);
        assert!(
            !first_frame_output_text.is_empty(),
            "{output_kind:?} wrote no pixels"
        );

        output_state.prepare_frame(
            &[build_placed_image_paint(
                &source_output_paint,
                source_output_paint.placement_key,
                Rect::new(0, 3, 1, 1),
            )],
            Some(cell_snapshot),
            Some(cell_size),
        );
        assert!(
            !output_state.is_work_pending(),
            "{output_kind:?} started another encode"
        );
        let (moved_frame_reset_text, moved_frame_output_text) =
            paint_prepared_frame(&mut output_state);
        assert_eq!(moved_frame_reset_text, "\x1b[2J", "{output_kind:?}");
        assert_eq!(
            moved_frame_output_text.replacen("\x1b[4;1H", "\x1b[1;1H", 1),
            first_frame_output_text,
            "{output_kind:?} re-encoded the moved image"
        );
    }
}

#[test]
fn a_partly_transparent_iterm_image_that_moves_reuses_its_encode_over_equal_cells_only() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let build_image_paint = |row_index: u16| {
        build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, row_index, 1, 1),
        )
    };
    let mut red_cell_style = Style::default();
    red_cell_style.set_background_color(Color::Rgb(255, 0, 0));
    let mut cell_states = vec![ImageCellState::default(); 64];
    cell_states[6 * 8] = ImageCellState {
        style: red_cell_style,
        ..ImageCellState::default()
    };
    let cell_snapshot = Some(Arc::new(build_image_cell_snapshot(
        Rect::new(0, 0, 8, 8),
        cell_states,
    )));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Iterm);
    let (first_worker_request, _, _) = encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &[build_image_paint(0)],
        cell_snapshot.clone(),
        cell_size,
    );

    output_state.prepare_frame(&[build_image_paint(3)], cell_snapshot.clone(), cell_size);

    assert_eq!(
        worker_request_receiver.try_recv().err(),
        Some(TryRecvError::Empty),
        "the move over equal cells encoded again"
    );
    assert_eq!(
        paint_prepared_frame(&mut output_state),
        (
            String::from("\x1b[2J"),
            build_unit_frame_output(ImageOutputKind::Iterm, 3, "unit 1:1")
        )
    );

    output_state.prepare_frame(&[build_image_paint(6)], cell_snapshot, cell_size);

    let red_cell_worker_request = worker_request_receiver
        .try_recv()
        .expect("the move over a red cell reaches the worker");
    assert_ne!(
        red_cell_worker_request.encode_keys,
        first_worker_request.encode_keys
    );
}

#[test]
fn an_unchanged_frame_after_a_commit_starts_no_work_and_commits_nothing() {
    for output_kind in [ImageOutputKind::Kitty, ImageOutputKind::Iterm] {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        );
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let build_cell_snapshot = || Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);

        encode_and_paint_frame(
            &mut output_state,
            &worker_request_receiver,
            &worker_message_sender,
            std::slice::from_ref(&image_paint),
            build_cell_snapshot(),
            cell_size,
        );

        output_state.prepare_frame(
            std::slice::from_ref(&image_paint),
            build_cell_snapshot(),
            cell_size,
        );
        assert_eq!(
            worker_request_receiver.try_recv().err(),
            Some(TryRecvError::Empty),
            "{output_kind:?} started a job"
        );
        assert!(
            !output_state.is_native_commit_pending(),
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
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let image_paint = build_placed_image_paint(
        &source_output_paint,
        source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    );
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
    let layout_area = Rect::new(0, 0, 2, 1);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Iterm);
    let (_, _, first_frame_output_text) = encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        std::slice::from_ref(&image_paint),
        Some(Arc::new(build_blank_snapshot(layout_area))),
        cell_size,
    );

    let cell_snapshot_with_glyph = build_image_cell_snapshot(
        layout_area,
        vec![
            ImageCellState {
                character: 'B',
                ..ImageCellState::default()
            },
            ImageCellState::default(),
        ],
    );
    output_state.prepare_frame(
        std::slice::from_ref(&image_paint),
        Some(Arc::new(cell_snapshot_with_glyph)),
        cell_size,
    );

    assert_eq!(
        worker_request_receiver.try_recv().err(),
        Some(TryRecvError::Empty),
        "the glyph started an encode"
    );
    assert!(
        output_state.is_native_commit_pending(),
        "the image is not written again"
    );
    assert!(
        !output_state.needs_screen_reset,
        "an unmoved image reset the screen"
    );
    assert_eq!(
        paint_prepared_frame(&mut output_state),
        (String::new(), first_frame_output_text)
    );
}

#[test]
fn an_iterm_reset_clears_the_screen_and_a_kitty_reset_does_not() {
    let mut iterm_output_state = ImageOutputState::from_output_kind(None, Arc::new(|| {}));
    iterm_output_state.output_kind = Some(ImageOutputKind::Iterm);
    iterm_output_state.needs_screen_reset = true;
    let mut iterm_frame_reset_bytes = Vec::new();
    assert!(iterm_output_state
        .write_frame_reset(&mut iterm_frame_reset_bytes)
        .expect("the frame reset writes"));
    assert_eq!(iterm_frame_reset_bytes, b"\x1b[2J");

    let (mut kitty_output_state, _worker_request_receiver, _worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    kitty_output_state.needs_screen_reset = true;
    let mut kitty_frame_reset_bytes = Vec::new();
    assert!(!kitty_output_state
        .write_frame_reset(&mut kitty_frame_reset_bytes)
        .expect("the frame reset writes"));
    assert_eq!(kitty_frame_reset_bytes, b"\x1b_Ga=d,d=a,q=2;\x1b\\");
}

#[test]
fn a_failed_iterm_frame_frees_no_kitty_image() {
    let mut output_state = ImageOutputState::from_output_kind(None, Arc::new(|| {}));
    output_state.output_kind = Some(ImageOutputKind::Iterm);

    output_state.fail_frame_commit();

    assert!(!output_state.should_free_all_kitty_images);
}

#[test]
fn a_kitty_paint_skips_the_alpha_scan_and_an_iterm_paint_runs_it() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let image_paint = build_placed_image_paint(
        &source_output_paint,
        source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    );
    let (mut kitty_output_state, _kitty_worker_request_receiver, _kitty_worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    kitty_output_state.prepare_frame(std::slice::from_ref(&image_paint), None, None);
    assert_eq!(kitty_output_state.latest_output_paints[0].alpha_stats, None);

    let (mut iterm_output_state, _iterm_worker_request_receiver, _iterm_worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Iterm);
    iterm_output_state.prepare_frame(
        std::slice::from_ref(&image_paint),
        Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8)))),
        PixelCellSize::from_pixel_dimensions(1, 1),
    );
    assert_eq!(
        iterm_output_state.latest_output_paints[0].alpha_stats,
        Some(AlphaStats {
            has_zero: false,
            has_partial: true,
        })
    );
}

#[test]
fn an_iterm_host_resize_frees_no_kitty_image_and_clears_no_key() {
    let mut output_state = ImageOutputState::from_output_kind(None, Arc::new(|| {}));
    output_state.output_kind = Some(ImageOutputKind::Iterm);
    let encode_key = build_output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell"),
        &build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
    );
    output_state.latest_encode_inputs.encode_keys = vec![encode_key.clone()];
    output_state.set_host_terminal_size(80, 24);

    output_state.set_host_terminal_size(100, 30);

    assert!(!output_state.should_free_all_kitty_images);
    assert_eq!(output_state.latest_encode_inputs.encode_keys, [encode_key]);
}

/// Prepare `image_paints` on `output_state` over `cell_snapshot` with
/// `cell_size`, finish the request the frame starts, prepare the frame again,
/// and paint it. A Kitty request finishes with
/// [`finish_request_with_kitty_uploads`], and an iTerm2 or Sixel request with
/// [`finish_request_with_units`]. Returns the request and the paint's reset
/// and output text.
fn encode_and_paint_frame(
    output_state: &mut ImageOutputState,
    worker_request_receiver: &mpsc::Receiver<WorkerRequest>,
    worker_message_sender: &mpsc::Sender<WorkerMessage>,
    image_paints: &[ImagePaint],
    cell_snapshot: Option<Arc<ImageCellSnapshot>>,
    cell_size: Option<PixelCellSize>,
) -> (WorkerRequest, String, String) {
    output_state.prepare_frame(image_paints, cell_snapshot.clone(), cell_size);
    let worker_request = worker_request_receiver
        .try_recv()
        .expect("the frame reaches the worker");
    if output_state.output_kind == Some(ImageOutputKind::Kitty) {
        finish_request_with_kitty_uploads(worker_message_sender, &worker_request);
    } else {
        finish_request_with_units(worker_message_sender, &worker_request);
    }
    output_state.prepare_frame(image_paints, cell_snapshot, cell_size);
    let (frame_reset_text, frame_output_text) = paint_prepared_frame(output_state);
    (worker_request, frame_reset_text, frame_output_text)
}

/// The placement of each paint `worker_request` encodes, in draw order.
fn list_requested_placement_keys(worker_request: &WorkerRequest) -> Vec<ImagePlacementKey> {
    worker_request
        .list_requested_encode_keys()
        .into_iter()
        .map(|(placement_key, _)| placement_key)
        .collect()
}

#[test]
fn kitty_uploads_one_image_once_and_places_it_again_when_it_moves() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);

    let (worker_request, frame_reset_text, frame_output_text) = encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &[build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        )],
        None,
        None,
    );
    assert_eq!(
        worker_request.list_requested_kitty_images(),
        [(source_output_paint.image_content_id, 1)]
    );
    assert_eq!(frame_reset_text, "");
    assert_eq!(
        frame_output_text,
        "upload 1;\x1b[1;1H\x1b_Ga=p,i=1,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l"
    );

    output_state.prepare_frame(
        &[build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 5, 1, 1),
        )],
        None,
        None,
    );

    assert_eq!(
        worker_request_receiver.try_recv().err(),
        Some(TryRecvError::Empty),
        "the moved image was uploaded again"
    );
    assert_eq!(
        paint_prepared_frame(&mut output_state),
        (
            String::from("\x1b_Ga=d,d=a,q=2;\x1b\\"),
            String::from(
                "\x1b[6;1H\x1b_Ga=p,i=1,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l"
            )
        )
    );
}

#[test]
fn kitty_frees_one_image_id_after_its_content_leaves_the_frame() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &[build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        )],
        None,
        None,
    );

    output_state.prepare_frame(&[], None, None);

    assert_eq!(output_state.kitty_image_ids_to_delete, vec![1]);
    assert_eq!(
        paint_prepared_frame(&mut output_state),
        (
            String::from("\x1b_Ga=d,d=a,q=2;\x1b\\\x1b_Ga=d,d=I,i=1,q=2;\x1b\\"),
            String::new()
        )
    );
    assert_eq!(output_state.kitty_image_ids_to_delete, Vec::<u32>::new());
}

#[test]
fn a_failed_kitty_frame_frees_every_image_the_host_holds() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &[build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        )],
        None,
        None,
    );

    output_state.fail_frame_commit();

    assert!(output_state.should_free_all_kitty_images);
    assert!(output_state.held_kitty_image_id_by_content_id.is_empty());
    let mut frame_reset_bytes = Vec::new();
    assert!(!output_state
        .write_frame_reset(&mut frame_reset_bytes)
        .expect("the frame reset writes"));
    assert_eq!(frame_reset_bytes, b"\x18\x1b\\\x1b_Ga=d,d=A,q=2;\x1b\\");
    assert!(!output_state.should_free_all_kitty_images);
}

#[test]
fn a_new_kitty_image_shows_the_older_image_until_its_upload_is_written() {
    let first_source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let second_source_output_paint = build_output_paint(vec![0, 255, 0, 255], 1, 1, 0);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &[build_placed_image_paint(
            &first_source_output_paint,
            first_source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        )],
        None,
        None,
    );
    let second_image_paints = [build_placed_image_paint(
        &second_source_output_paint,
        first_source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    )];

    output_state.prepare_frame(&second_image_paints, None, None);
    let second_worker_request = worker_request_receiver
        .try_recv()
        .expect("the new image reaches the worker");
    assert_eq!(
        second_worker_request.list_requested_kitty_images(),
        [(second_source_output_paint.image_content_id, 2)]
    );
    assert_eq!(output_state.kitty_image_ids_to_delete, Vec::<u32>::new());
    assert_eq!(
        paint_prepared_frame(&mut output_state),
        (String::new(), String::new()),
        "the older image left the screen while the new one uploads"
    );

    finish_request_with_kitty_uploads(&worker_message_sender, &second_worker_request);
    output_state.prepare_frame(&second_image_paints, None, None);
    assert_eq!(
        paint_prepared_frame(&mut output_state),
        (
            String::from("\x1b_Ga=d,d=a,q=2;\x1b\\\x1b_Ga=d,d=I,i=1,q=2;\x1b\\"),
            String::from(
                "upload 2;\x1b[1;1H\x1b_Ga=p,i=2,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l"
            )
        )
    );
}

#[test]
fn returning_to_the_frame_the_active_encode_runs_drops_the_waiting_request() {
    for output_kind in list_cell_output_kinds() {
        let placement_key = (PaneId::new(), 1);
        let [first_image_paint, second_image_paint] = [
            build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
            build_output_paint(vec![0, 255, 0, 255], 1, 1, 0),
        ]
        .map(|source_output_paint| {
            build_placed_image_paint(&source_output_paint, placement_key, Rect::new(0, 0, 1, 1))
        });
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, _worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        output_state.prepare_frame(
            std::slice::from_ref(&first_image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        let first_worker_request = worker_request_receiver
            .try_recv()
            .expect("the first frame reaches the worker");
        output_state.prepare_frame(
            std::slice::from_ref(&second_image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        let second_cancellation_token = Arc::clone(
            &output_state
                .pending_worker_request
                .as_ref()
                .expect("the second frame waits")
                .is_cancellation_requested,
        );

        output_state.prepare_frame(
            std::slice::from_ref(&first_image_paint),
            cell_snapshot,
            cell_size,
        );

        assert!(
            output_state.pending_worker_request.is_none(),
            "{output_kind:?} queued the frame the active encode runs"
        );
        assert!(
            second_cancellation_token.load(Ordering::Acquire),
            "{output_kind:?}"
        );
        assert!(
            !first_worker_request
                .is_cancellation_requested
                .load(Ordering::Acquire),
            "{output_kind:?}"
        );
        assert_eq!(
            worker_request_receiver.try_recv().err(),
            Some(TryRecvError::Empty),
            "{output_kind:?}"
        );
    }
}

#[test]
fn a_waiting_request_for_the_same_frame_stays_when_the_frame_is_picked_again() {
    for output_kind in list_cell_output_kinds() {
        let placement_key = (PaneId::new(), 1);
        let [first_image_paint, second_image_paint] = [
            build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
            build_output_paint(vec![0, 255, 0, 255], 1, 1, 0),
        ]
        .map(|source_output_paint| {
            build_placed_image_paint(&source_output_paint, placement_key, Rect::new(0, 0, 1, 1))
        });
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, _worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        output_state.prepare_frame(
            std::slice::from_ref(&first_image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        worker_request_receiver
            .try_recv()
            .expect("the first frame reaches the worker");
        output_state.prepare_frame(
            std::slice::from_ref(&second_image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        let waiting_frame_generation = output_state
            .pending_worker_request
            .as_ref()
            .expect("the second frame waits")
            .frame_generation;

        output_state.fail_frame_commit();
        output_state.prepare_frame(
            std::slice::from_ref(&second_image_paint),
            cell_snapshot,
            cell_size,
        );

        let pending_worker_request = output_state
            .pending_worker_request
            .as_ref()
            .expect("the second frame still waits");
        assert_eq!(
            pending_worker_request.frame_generation, waiting_frame_generation,
            "{output_kind:?} replaced the waiting request"
        );
        assert!(
            !pending_worker_request
                .is_cancellation_requested
                .load(Ordering::Acquire),
            "{output_kind:?}"
        );
    }
}

#[test]
fn a_worker_that_is_gone_fails_the_waiting_request_too() {
    let placement_key = (PaneId::new(), 1);
    let [first_image_paint, second_image_paint] = [
        build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
        build_output_paint(vec![0, 255, 0, 255], 1, 1, 0),
    ]
    .map(|source_output_paint| {
        build_placed_image_paint(&source_output_paint, placement_key, Rect::new(0, 0, 1, 1))
    });
    let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Iterm);
    output_state.prepare_frame(
        std::slice::from_ref(&first_image_paint),
        cell_snapshot.clone(),
        cell_size,
    );
    output_state.prepare_frame(
        std::slice::from_ref(&second_image_paint),
        cell_snapshot.clone(),
        cell_size,
    );
    assert_eq!(
        output_state
            .pending_worker_request
            .as_ref()
            .map(|pending_worker_request| pending_worker_request.frame_generation),
        Some(2)
    );

    drop(worker_request_receiver);
    drop(worker_message_sender);
    output_state.prepare_frame(
        std::slice::from_ref(&second_image_paint),
        cell_snapshot,
        cell_size,
    );

    assert_eq!(
        output_state.list_unavailable_placement_keys(),
        [placement_key]
    );
    assert!(!output_state.is_work_pending());
}

#[test]
fn a_new_kitty_image_with_another_source_size_shows_no_pixels_until_its_upload_is_written() {
    let first_source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let second_source_output_paint = build_output_paint([0, 255, 0, 255].repeat(4), 2, 2, 0);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &[build_placed_image_paint(
            &first_source_output_paint,
            first_source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        )],
        None,
        None,
    );
    let second_image_paints = [build_placed_image_paint(
        &second_source_output_paint,
        first_source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    )];

    output_state.prepare_frame(&second_image_paints, None, None);

    worker_request_receiver
        .try_recv()
        .expect("the new image reaches the worker");
    assert_eq!(
        paint_prepared_frame(&mut output_state),
        (
            String::from("\x1b_Ga=d,d=a,q=2;\x1b\\\x1b_Ga=d,d=I,i=1,q=2;\x1b\\"),
            String::new()
        )
    );
    assert_eq!(output_state.list_unavailable_placement_keys(), []);
}

#[test]
fn a_kitty_image_whose_upload_runs_is_not_uploaded_again_when_it_moves() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let (mut output_state, worker_request_receiver, _worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    output_state.prepare_frame(
        &[build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        )],
        None,
        None,
    );
    worker_request_receiver
        .try_recv()
        .expect("the image reaches the worker");

    output_state.prepare_frame(
        &[build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 2, 1, 1),
        )],
        None,
        None,
    );

    assert_eq!(
        worker_request_receiver.try_recv().err(),
        Some(TryRecvError::Empty)
    );
    assert!(output_state.pending_worker_request.is_none());
}

#[test]
fn a_new_kitty_image_skips_the_ids_of_the_running_and_the_waiting_uploads() {
    let pane_id = PaneId::new();
    let [first_image_paint, second_image_paint, third_image_paint] = [
        (vec![255, 0, 0, 255], 1),
        (vec![0, 255, 0, 255], 2),
        (vec![0, 0, 255, 255], 3),
    ]
    .map(|(rgba_bytes, placement_id)| {
        build_placed_image_paint(
            &build_output_paint(rgba_bytes, 1, 1, 0),
            (pane_id, placement_id),
            Rect::new(0, placement_id as u16, 1, 1),
        )
    });
    let [first_image_content_id, second_image_content_id, third_image_content_id] =
        [&first_image_paint, &second_image_paint, &third_image_paint]
            .map(|image_paint| image_paint.image_content_id);
    let (mut output_state, worker_request_receiver, _worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    output_state.prepare_frame(std::slice::from_ref(&first_image_paint), None, None);
    let first_worker_request = worker_request_receiver
        .try_recv()
        .expect("the first image reaches the worker");
    output_state.prepare_frame(
        &[first_image_paint.clone(), second_image_paint.clone()],
        None,
        None,
    );
    output_state.next_kitty_image_id = 1;

    output_state.prepare_frame(
        &[first_image_paint, second_image_paint, third_image_paint],
        None,
        None,
    );

    assert_eq!(
        first_worker_request.list_requested_kitty_images(),
        [(first_image_content_id, 1)]
    );
    assert_eq!(
        output_state
            .pending_worker_request
            .as_ref()
            .expect("the new images wait")
            .list_requested_kitty_images(),
        [(second_image_content_id, 2), (third_image_content_id, 3)]
    );
}

#[test]
fn a_waiting_kitty_upload_stays_when_the_same_frame_is_picked_again() {
    let pane_id = PaneId::new();
    let [first_image_paint, second_image_paint] =
        [(vec![255, 0, 0, 255], 1), (vec![0, 255, 0, 255], 2)].map(|(rgba_bytes, placement_id)| {
            build_placed_image_paint(
                &build_output_paint(rgba_bytes, 1, 1, 0),
                (pane_id, placement_id),
                Rect::new(0, placement_id as u16, 1, 1),
            )
        });
    let image_paints = [first_image_paint.clone(), second_image_paint];
    let (mut output_state, worker_request_receiver, _worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    output_state.prepare_frame(std::slice::from_ref(&first_image_paint), None, None);
    worker_request_receiver
        .try_recv()
        .expect("the first image reaches the worker");
    output_state.prepare_frame(&image_paints, None, None);
    let waiting_frame_generation = output_state
        .pending_worker_request
        .as_ref()
        .expect("the second image waits")
        .frame_generation;

    output_state.fail_frame_commit();
    output_state.prepare_frame(&image_paints, None, None);

    let pending_worker_request = output_state
        .pending_worker_request
        .as_ref()
        .expect("the second image still waits");
    assert_eq!(
        pending_worker_request.frame_generation,
        waiting_frame_generation
    );
    assert!(!pending_worker_request
        .is_cancellation_requested
        .load(Ordering::Acquire));
}

#[test]
fn a_failed_frame_clears_the_host_screen_before_its_images_are_drawn_again() {
    for output_kind in list_cell_output_kinds() {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        );
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        encode_and_paint_frame(
            &mut output_state,
            &worker_request_receiver,
            &worker_message_sender,
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );

        output_state.fail_frame_commit();
        output_state.prepare_frame(std::slice::from_ref(&image_paint), cell_snapshot, cell_size);

        assert_eq!(
            worker_request_receiver.try_recv().err(),
            Some(TryRecvError::Empty),
            "{output_kind:?}"
        );
        assert_eq!(
            paint_prepared_frame(&mut output_state),
            (
                String::from("\x18\x1b\\\x1b[2J"),
                build_unit_frame_output(output_kind, 0, "unit 1:1")
            ),
            "{output_kind:?}"
        );
    }
}

#[test]
fn returning_to_the_shown_pixels_before_a_newer_result_is_painted_encodes_nothing() {
    for output_kind in list_cell_output_kinds() {
        let placement_key = (PaneId::new(), 1);
        let [first_image_paint, second_image_paint] = [
            build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
            build_output_paint(vec![0, 255, 0, 255], 1, 1, 0),
        ]
        .map(|source_output_paint| {
            build_placed_image_paint(&source_output_paint, placement_key, Rect::new(0, 0, 1, 1))
        });
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        encode_and_paint_frame(
            &mut output_state,
            &worker_request_receiver,
            &worker_message_sender,
            std::slice::from_ref(&first_image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        output_state.prepare_frame(
            std::slice::from_ref(&second_image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        let second_worker_request = worker_request_receiver
            .try_recv()
            .expect("the second frame reaches the worker");
        finish_request_with_units(&worker_message_sender, &second_worker_request);

        output_state.prepare_frame(
            std::slice::from_ref(&first_image_paint),
            cell_snapshot,
            cell_size,
        );

        assert_eq!(
            worker_request_receiver.try_recv().err(),
            Some(TryRecvError::Empty),
            "{output_kind:?}"
        );
        assert!(
            output_state.pending_worker_request.is_none(),
            "{output_kind:?}"
        );
        assert_eq!(
            paint_prepared_frame(&mut output_state),
            (String::new(), String::new()),
            "{output_kind:?} wrote the pixels the host shows again"
        );
    }
}

#[test]
fn an_encode_result_is_dropped_when_its_placement_leaves_the_frame() {
    for output_kind in list_cell_output_kinds() {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        );
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        output_state.prepare_frame(
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        let worker_request = worker_request_receiver
            .try_recv()
            .expect("the frame reaches the worker");
        finish_request_with_units(&worker_message_sender, &worker_request);
        output_state.prepare_frame(
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        assert_eq!(
            output_state
                .encoded_placement_by_key
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [source_output_paint.placement_key],
            "{output_kind:?}"
        );

        output_state.prepare_frame(&[], cell_snapshot, cell_size);

        assert!(
            output_state.encoded_placement_by_key.is_empty(),
            "{output_kind:?}"
        );
    }
}

#[test]
fn a_failed_kitty_upload_shows_the_unavailable_text_and_uploads_again_after_the_image_returns() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let image_paints = [build_placed_image_paint(
        &source_output_paint,
        source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    )];
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);

    output_state.prepare_frame(&image_paints, None, None);
    let worker_request = worker_request_receiver
        .try_recv()
        .expect("the frame reaches the worker");
    finish_request(&worker_message_sender, &worker_request, true);
    output_state.prepare_frame(&image_paints, None, None);
    assert_eq!(
        output_state.list_unavailable_placement_keys(),
        [source_output_paint.placement_key]
    );
    assert_eq!(
        worker_request_receiver.try_recv().err(),
        Some(TryRecvError::Empty)
    );
    assert!(output_state.held_kitty_image_id_by_content_id.is_empty());

    output_state.prepare_frame(&[], None, None);
    output_state.prepare_frame(&image_paints, None, None);
    let retried_worker_request = worker_request_receiver
        .try_recv()
        .expect("the returned image reaches the worker");
    assert_eq!(
        retried_worker_request.list_requested_kitty_images(),
        [(source_output_paint.image_content_id, 2)]
    );
}

#[test]
fn a_host_resize_writes_every_kitty_image_again_from_its_kept_payload() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let image_paints = [build_placed_image_paint(
        &source_output_paint,
        source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    )];
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    output_state.set_host_terminal_size(80, 24);
    encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &image_paints,
        None,
        None,
    );

    output_state.set_host_terminal_size(80, 24);
    output_state.prepare_frame(&image_paints, None, None);
    assert_eq!(
        worker_request_receiver.try_recv().err(),
        Some(TryRecvError::Empty),
        "an unchanged size keeps the upload"
    );

    output_state.set_host_terminal_size(100, 24);
    assert!(output_state.should_free_all_kitty_images);
    output_state.prepare_frame(&image_paints, None, None);

    let resized_worker_request = worker_request_receiver
        .try_recv()
        .expect("the resized frame reaches the worker");
    assert_eq!(
        resized_worker_request.list_requested_kitty_images(),
        [(source_output_paint.image_content_id, 2)]
    );
    assert_eq!(
        resized_worker_request.kitty_upload_requests[0]
            .compressed_bytes
            .as_deref(),
        Some(format!("zlib {}", source_output_paint.image_content_id).as_bytes())
    );
    run_worker_job(
        &resized_worker_request,
        &worker_message_sender,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
    .expect("the worker writes the transmit");
    finish_request(&worker_message_sender, &resized_worker_request, false);
    output_state.prepare_frame(&image_paints, None, None);
    let mut expected_upload_bytes = Vec::new();
    write_kitty_transmit(
        &mut expected_upload_bytes,
        1,
        1,
        2,
        format!("zlib {}", source_output_paint.image_content_id).as_bytes(),
    )
    .expect("the transmit writes");
    assert_eq!(
        paint_prepared_frame(&mut output_state),
        (
            String::from("\x1b_Ga=d,d=A,q=2;\x1b\\"),
            format!(
                "{}\x1b[1;1H\x1b_Ga=p,i=2,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l",
                String::from_utf8(expected_upload_bytes).expect("the transmit is text")
            )
        )
    );
}

#[test]
fn the_first_host_size_forgets_nothing() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &[build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        )],
        None,
        None,
    );
    assert_eq!(
        output_state.held_kitty_image_id_by_content_id,
        HashMap::from([(source_output_paint.image_content_id, 1)])
    );

    output_state.set_host_terminal_size(80, 24);

    assert_eq!(
        output_state.held_kitty_image_id_by_content_id,
        HashMap::from([(source_output_paint.image_content_id, 1)])
    );
    assert!(!output_state.should_free_all_kitty_images);
}

#[test]
fn a_kitty_reset_with_only_departed_ids_frees_them_and_keeps_the_placements() {
    let (mut output_state, _worker_request_receiver, _worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    output_state.kitty_image_ids_to_delete = vec![3, 7];
    let mut frame_reset_bytes = Vec::new();

    assert!(!output_state
        .write_frame_reset(&mut frame_reset_bytes)
        .expect("the frame reset writes"));

    assert_eq!(
        frame_reset_bytes,
        b"\x1b_Ga=d,d=I,i=3,q=2;\x1b\\\x1b_Ga=d,d=I,i=7,q=2;\x1b\\"
    );
    assert_eq!(output_state.kitty_image_ids_to_delete, Vec::<u32>::new());
}

#[test]
fn two_paints_of_one_image_in_one_frame_upload_it_once() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);

    let second_image_paint = build_placed_image_paint(
        &source_output_paint,
        (source_output_paint.placement_key.0, 2),
        Rect::new(0, 3, 1, 1),
    );
    let (worker_request, _, frame_output_text) = encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &[
            build_placed_image_paint(
                &source_output_paint,
                source_output_paint.placement_key,
                Rect::new(0, 0, 1, 1),
            ),
            second_image_paint,
        ],
        None,
        None,
    );

    assert_eq!(
        worker_request.list_requested_kitty_images(),
        [(source_output_paint.image_content_id, 1)]
    );
    assert_eq!(
        frame_output_text,
        "upload 1;\x1b[1;1H\x1b_Ga=p,i=1,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[4;1H\x1b_Ga=p,i=1,p=2,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l"
    );
    assert_eq!(
        output_state.held_kitty_image_id_by_content_id,
        HashMap::from([(source_output_paint.image_content_id, 1)])
    );
    assert_eq!(output_state.next_kitty_image_id, 2);
}

#[test]
fn kitty_uploads_past_the_frame_limit_wait_for_the_next_paint() {
    let first_source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let second_source_output_paint = build_output_paint(vec![0, 255, 0, 255], 1, 1, 0);
    let image_paints = [
        build_placed_image_paint(
            &first_source_output_paint,
            first_source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        ),
        build_placed_image_paint(
            &second_source_output_paint,
            (first_source_output_paint.placement_key.0, 2),
            Rect::new(0, 3, 1, 1),
        ),
    ];
    let (mut output_state, worker_request_receiver, _worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    let image_output_ready_count = Arc::new(AtomicUsize::new(0));
    let notified_image_output_ready_count = Arc::clone(&image_output_ready_count);
    output_state.notify_image_output_ready = Arc::new(move || {
        notified_image_output_ready_count.fetch_add(1, Ordering::Relaxed);
    });
    output_state.prepare_frame(&image_paints, None, None);
    let worker_request = worker_request_receiver
        .try_recv()
        .expect("the frame reaches the worker");
    let largest_fitting_upload_bytes: Arc<[u8]> = Arc::from(vec![
        b'x';
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT
            - FRAME_FRAMING_BYTE_COUNT
            - 2 * UNIT_FRAMING_BYTE_COUNT
    ]);
    let one_byte_upload_bytes: Arc<[u8]> = Arc::from(vec![b'x']);
    output_state.active_job = None;
    output_state.finished_kitty_uploads = worker_request
        .kitty_upload_requests
        .iter()
        .zip([largest_fitting_upload_bytes, one_byte_upload_bytes])
        .map(|(kitty_upload_request, upload_bytes)| FinishedKittyUpload {
            image_content_id: kitty_upload_request.image_content_id,
            kitty_image_id: kitty_upload_request.kitty_image_id,
            upload_bytes,
        })
        .collect();
    output_state.has_unselected_results = true;

    output_state.prepare_frame(&image_paints, None, None);
    assert_eq!(output_state.selected_kitty_upload_ids, [1]);
    assert_eq!(
        worker_request_receiver.try_recv().err(),
        Some(TryRecvError::Empty)
    );
    output_state.commit_frame();
    assert!(output_state.has_unselected_results());
    assert_eq!(image_output_ready_count.load(Ordering::Relaxed), 1);

    output_state.prepare_frame(&image_paints, None, None);
    assert_eq!(output_state.selected_kitty_upload_ids, [2]);
    assert_eq!(
        output_state
            .selected_placements
            .iter()
            .map(|selected_placement| selected_placement.placement_key)
            .collect::<Vec<_>>(),
        [
            (image_paints[0].pane_id, image_paints[0].placement_id),
            (image_paints[1].pane_id, image_paints[1].placement_id)
        ]
    );
    output_state.commit_frame();
    assert!(!output_state.has_unselected_results());
    assert_eq!(image_output_ready_count.load(Ordering::Relaxed), 1);
}

#[test]
fn the_first_kitty_image_id_of_a_new_output_is_between_one_and_the_largest_id() {
    let first_kitty_image_ids = (0..1_000)
        .map(|_| ImageOutputState::from_output_kind(None, Arc::new(|| {})).next_kitty_image_id)
        .collect::<Vec<_>>();

    for first_kitty_image_id in &first_kitty_image_ids {
        assert!(
            (1..=MAX_KITTY_IMAGE_ID).contains(first_kitty_image_id),
            "{first_kitty_image_id}"
        );
    }
    assert!(first_kitty_image_ids
        .iter()
        .any(|first_kitty_image_id| *first_kitty_image_id != first_kitty_image_ids[0]));
}

#[test]
fn a_new_kitty_image_goes_to_one_after_the_largest_id_and_skips_every_held_id() {
    let first_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let second_output_paint = build_output_paint(vec![0, 255, 0, 255], 1, 1, 0);
    let build_image_paint = |source_output_paint: &OutputPaint, image_content_id: u64| {
        ImagePaint::from_image_placement(
            source_output_paint.placement_key.0,
            image_content_id,
            image_content_id,
            Arc::clone(&source_output_paint.image_record),
            Rect::new(0, 0, 1, 1),
            source_output_paint.source_rect,
            0,
        )
    };
    let (mut output_state, worker_request_receiver, _worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    output_state.next_kitty_image_id = MAX_KITTY_IMAGE_ID;
    output_state.held_kitty_image_id_by_content_id.insert(9, 1);
    output_state.kitty_image_ids_to_delete = vec![2];

    output_state.prepare_frame(
        &[
            build_image_paint(&first_output_paint, 1),
            build_image_paint(&second_output_paint, 2),
        ],
        None,
        None,
    );
    let worker_request = worker_request_receiver
        .try_recv()
        .expect("the frame reaches the worker");

    assert_eq!(
        worker_request.list_requested_kitty_images(),
        [(1, MAX_KITTY_IMAGE_ID), (2, 3)]
    );
    assert_eq!(output_state.next_kitty_image_id, 4);
    assert_eq!(output_state.kitty_image_ids_to_delete, vec![2, 1]);
    assert!(!output_state.should_free_all_kitty_images);
}

/// The images and placements a terminal holds when it acts on a Kitty delete
/// only by image id (`d=i`, `d=I`) and on delete all (`d=a`, `d=A`), as
/// WezTerm does. Every other delete changes nothing.
#[derive(Debug, Default)]
struct KittyDeleteByIdTerminal {
    held_image_ids: HashSet<u32>,
    held_placement_ids: HashSet<(u32, u32)>,
}

impl KittyDeleteByIdTerminal {
    /// Apply every Kitty command in `host_output_bytes`. Panics on a command
    /// that carries `I=`, and on a placement of an image the terminal does not
    /// hold.
    fn apply_host_output(&mut self, host_output_bytes: &[u8]) {
        let host_output_text =
            std::str::from_utf8(host_output_bytes).expect("Kitty output is ASCII");
        for kitty_command_text in host_output_text.split("\x1b_G").skip(1) {
            let (kitty_control_text, _) = kitty_command_text
                .split_once(';')
                .expect("every Kitty command has a payload separator");
            let kitty_control_by_key = kitty_control_text
                .split(',')
                .map(|kitty_control_pair| {
                    kitty_control_pair
                        .split_once('=')
                        .expect("every Kitty control is a key and a value")
                })
                .collect::<HashMap<_, _>>();
            assert!(
                !kitty_control_by_key.contains_key("I"),
                "{kitty_control_text}"
            );
            let parse_control_number = |control_key: &str| -> u32 {
                kitty_control_by_key[control_key]
                    .parse()
                    .expect("a Kitty numeric control is decimal")
            };
            match kitty_control_by_key.get("a").copied() {
                Some("t") => {
                    self.held_image_ids.insert(parse_control_number("i"));
                }
                Some("p") => {
                    let kitty_image_id = parse_control_number("i");
                    assert!(
                        self.held_image_ids.contains(&kitty_image_id),
                        "placement of image {kitty_image_id}, which the terminal does not hold"
                    );
                    self.held_placement_ids
                        .insert((kitty_image_id, parse_control_number("p")));
                }
                Some("d") => match kitty_control_by_key["d"] {
                    "a" => self.held_placement_ids.clear(),
                    "A" => {
                        self.held_placement_ids.clear();
                        self.held_image_ids.clear();
                    }
                    "i" => {
                        let kitty_image_id = parse_control_number("i");
                        self.held_placement_ids
                            .retain(|(placed_image_id, _)| *placed_image_id != kitty_image_id);
                    }
                    "I" => {
                        let kitty_image_id = parse_control_number("i");
                        self.held_placement_ids
                            .retain(|(placed_image_id, _)| *placed_image_id != kitty_image_id);
                        self.held_image_ids.remove(&kitty_image_id);
                    }
                    _ => {}
                },
                None if kitty_control_by_key.contains_key("m") => {}
                other_action => panic!("unexpected Kitty action {other_action:?}"),
            }
        }
    }
}

#[test]
fn a_two_frame_animation_over_a_thousand_frames_compresses_each_frame_once_and_leaves_a_delete_by_id_terminal_holding_one_image(
) {
    let animation_frame_paints = [
        build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
        build_output_paint(vec![0, 0, 255, 255], 1, 1, 0),
    ];
    let placement_key = animation_frame_paints[0].placement_key;
    let build_animation_image_paint = |frame_index: u32| {
        let animation_frame_index = usize::try_from(frame_index % 2).expect("0 or 1");
        build_placed_image_paint(
            &animation_frame_paints[animation_frame_index],
            placement_key,
            Rect::new(0, 0, 1, 1),
        )
    };
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    let first_kitty_image_id = MAX_KITTY_IMAGE_ID - 499;
    output_state.next_kitty_image_id = first_kitty_image_id;
    let mut host_terminal = KittyDeleteByIdTerminal::default();

    for frame_index in 0..1_000u32 {
        let kitty_image_id = (first_kitty_image_id - 1 + frame_index) % MAX_KITTY_IMAGE_ID + 1;
        output_state.prepare_frame(&[build_animation_image_paint(frame_index)], None, None);
        let worker_request = worker_request_receiver
            .try_recv()
            .expect("each animation frame reaches the worker");
        let expected_compressed_bytes = (frame_index >= 2).then(|| {
            output_state
                .encoded_output_cache
                .find_kitty_compressed_bytes(
                    animation_frame_paints[usize::try_from(frame_index % 2).expect("0 or 1")]
                        .image_content_id,
                )
                .expect("the cache keeps the payload of the first loop")
        });
        assert_eq!(
            worker_request.kitty_upload_requests[0].compressed_bytes, expected_compressed_bytes,
            "frame {frame_index}"
        );
        let (waiting_frame_reset_text, waiting_frame_output_text) =
            paint_prepared_frame(&mut output_state);
        assert_eq!(
            (
                waiting_frame_reset_text.as_str(),
                waiting_frame_output_text.as_str()
            ),
            ("", ""),
            "frame {frame_index} changed the screen while its image uploads"
        );
        run_worker_job(
            &worker_request,
            &worker_message_sender,
            MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
        )
        .expect("the worker encodes");
        finish_request(&worker_message_sender, &worker_request, false);
        output_state.prepare_frame(&[build_animation_image_paint(frame_index)], None, None);
        let (frame_reset_text, frame_output_text) = paint_prepared_frame(&mut output_state);

        if frame_index == 0 {
            assert_eq!(frame_reset_text, "");
        } else {
            let previous_kitty_image_id =
                (kitty_image_id + MAX_KITTY_IMAGE_ID - 2) % MAX_KITTY_IMAGE_ID + 1;
            assert_eq!(
                frame_reset_text,
                format!(
                    "\x1b_Ga=d,d=a,q=2;\x1b\\\x1b_Ga=d,d=I,i={previous_kitty_image_id},q=2;\x1b\\"
                ),
                "frame {frame_index}"
            );
        }
        assert_eq!(
            frame_output_text.matches("\x1b_G").count(),
            2,
            "frame {frame_index}"
        );
        assert!(
            frame_output_text.contains(&format!(
                "\x1b_Ga=t,f=32,s=1,v=1,i={kitty_image_id},q=2,o=z,m=0;"
            )),
            "frame {frame_index}"
        );
        assert!(
            frame_output_text.contains(&format!("\x1b_Ga=p,i={kitty_image_id},p=1,")),
            "frame {frame_index}"
        );
        host_terminal.apply_host_output(frame_reset_text.as_bytes());
        host_terminal.apply_host_output(frame_output_text.as_bytes());
        assert_eq!(
            host_terminal.held_image_ids,
            HashSet::from([kitty_image_id]),
            "frame {frame_index}"
        );
        assert_eq!(
            host_terminal.held_placement_ids,
            HashSet::from([(kitty_image_id, 1)]),
            "frame {frame_index}"
        );
    }
}

/// One unit at offset `(0, 0)` of `output_byte_count` bytes.
fn build_cached_units(output_byte_count: usize) -> Arc<[TemplateUnit]> {
    Arc::from(vec![TemplateUnit {
        tile_offset: (0, 0),
        output_bytes: Arc::from(vec![b'x'; output_byte_count]),
    }])
}

/// The iTerm2 encode key of a new one-pixel image.
fn build_new_image_encode_key() -> EncodeKey {
    build_output_encode_key(
        ImageOutputKind::Iterm,
        PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell"),
        &build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
    )
}

/// Return whether `encoded_output_cache` keeps units for `encode_key`,
/// without marking them used.
fn is_units_cached(encoded_output_cache: &EncodedOutputCache, encode_key: &EncodeKey) -> bool {
    encoded_output_cache
        .cached_output_entry_by_key
        .contains_key(&CachedOutputKey::Units(encode_key.clone()))
}

#[test]
fn the_encode_cache_drops_the_least_recently_used_output_when_the_records_need_room() {
    let mut encoded_output_cache = EncodedOutputCache::new();
    let [first_encode_key, second_encode_key, third_encode_key] =
        [(); 3].map(|()| build_new_image_encode_key());
    for encode_key in [&first_encode_key, &second_encode_key, &third_encode_key] {
        encoded_output_cache
            .insert_cached_units(encode_key.clone(), build_cached_units(16 * 1024 * 1024));
    }
    assert_eq!(
        encoded_output_cache
            .find_cached_units(&first_encode_key)
            .map(|cached_units| {
                cached_units
                    .iter()
                    .map(|cached_unit| (cached_unit.tile_offset, cached_unit.output_bytes.len()))
                    .collect::<Vec<_>>()
            }),
        Some(vec![((0, 0), 16 * 1024 * 1024)])
    );

    encoded_output_cache.set_held_image_byte_count(32 * 1024 * 1024);

    assert!(is_units_cached(&encoded_output_cache, &first_encode_key));
    assert!(!is_units_cached(&encoded_output_cache, &second_encode_key));
    assert!(is_units_cached(&encoded_output_cache, &third_encode_key));
    assert_eq!(encoded_output_cache.cached_byte_count, 32 * 1024 * 1024);

    encoded_output_cache.set_held_image_byte_count(MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT);

    assert_eq!(encoded_output_cache.cached_output_entry_by_key.len(), 0);
    assert_eq!(encoded_output_cache.key_by_last_use_sequence.len(), 0);
    assert_eq!(encoded_output_cache.cached_byte_count, 0);
}

#[test]
fn an_output_larger_than_the_room_the_records_leave_is_not_kept() {
    let mut encoded_output_cache = EncodedOutputCache::new();
    let [kept_encode_key, oversized_encode_key, fitting_encode_key] =
        [(); 3].map(|()| build_new_image_encode_key());
    encoded_output_cache
        .insert_cached_units(kept_encode_key.clone(), build_cached_units(2 * 1024 * 1024));
    encoded_output_cache.set_held_image_byte_count(60 * 1024 * 1024);
    assert!(is_units_cached(&encoded_output_cache, &kept_encode_key));

    encoded_output_cache.insert_cached_units(
        oversized_encode_key.clone(),
        build_cached_units(5 * 1024 * 1024),
    );

    assert!(!is_units_cached(
        &encoded_output_cache,
        &oversized_encode_key
    ));
    assert!(is_units_cached(&encoded_output_cache, &kept_encode_key));

    encoded_output_cache.insert_cached_units(
        fitting_encode_key.clone(),
        build_cached_units(3 * 1024 * 1024),
    );

    assert!(is_units_cached(&encoded_output_cache, &fitting_encode_key));
    assert!(!is_units_cached(&encoded_output_cache, &kept_encode_key));
    assert_eq!(encoded_output_cache.cached_byte_count, 3 * 1024 * 1024);
}

#[test]
fn a_released_image_drops_its_outputs_and_each_composed_output_over_it() {
    let lower_output_paint = build_output_paint(vec![255, 0, 0, 128], 1, 1, 0);
    let upper_output_paint = build_output_paint(vec![0, 0, 255, 128], 1, 1, 1);
    let other_output_paint = build_output_paint(vec![0, 255, 0, 255], 1, 1, 0);
    let encode_keys = build_one_pixel_encode_inputs(
        ImageOutputKind::Iterm,
        &build_blank_snapshot(Rect::new(0, 0, 1, 1)),
        &[
            lower_output_paint.clone(),
            upper_output_paint,
            other_output_paint.clone(),
        ],
    )
    .encode_keys;
    let mut encoded_output_cache = EncodedOutputCache::new();
    for encode_key in &encode_keys {
        encoded_output_cache.insert_cached_units(encode_key.clone(), build_cached_units(4));
    }
    encoded_output_cache.insert_kitty_compressed_bytes(
        lower_output_paint.image_content_id,
        Arc::from(vec![1, 2, 3]),
    );
    encoded_output_cache.insert_kitty_compressed_bytes(
        other_output_paint.image_content_id,
        Arc::from(vec![4, 5, 6]),
    );

    encoded_output_cache.remove_released_outputs(&[lower_output_paint.image_content_id]);

    assert!(!is_units_cached(&encoded_output_cache, &encode_keys[0]));
    assert!(!is_units_cached(&encoded_output_cache, &encode_keys[1]));
    assert!(is_units_cached(&encoded_output_cache, &encode_keys[2]));
    assert_eq!(
        encoded_output_cache.find_kitty_compressed_bytes(lower_output_paint.image_content_id),
        None
    );
    assert_eq!(
        encoded_output_cache
            .find_kitty_compressed_bytes(other_output_paint.image_content_id)
            .as_deref(),
        Some(&[4, 5, 6][..])
    );
    assert_eq!(encoded_output_cache.cached_byte_count, 4 + 3);
}

#[test]
fn an_image_that_leaves_and_returns_draws_its_kept_units_until_the_session_releases_it() {
    for output_kind in list_cell_output_kinds() {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        );
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        encode_and_paint_frame(
            &mut output_state,
            &worker_request_receiver,
            &worker_message_sender,
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );
        output_state.prepare_frame(&[], cell_snapshot.clone(), cell_size);
        paint_prepared_frame(&mut output_state);

        output_state.prepare_frame(
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );

        assert_eq!(
            worker_request_receiver.try_recv().err(),
            Some(TryRecvError::Empty),
            "{output_kind:?} encoded the returning image again"
        );
        assert_eq!(
            paint_prepared_frame(&mut output_state),
            (
                String::new(),
                build_unit_frame_output(output_kind, 0, "unit 1:1")
            ),
            "{output_kind:?}"
        );

        output_state.prepare_frame(&[], cell_snapshot.clone(), cell_size);
        paint_prepared_frame(&mut output_state);
        output_state.release_image_contents(&[source_output_paint.image_content_id]);
        output_state.prepare_frame(std::slice::from_ref(&image_paint), cell_snapshot, cell_size);

        assert_eq!(
            worker_request_receiver
                .try_recv()
                .map(|worker_request| list_requested_placement_keys(&worker_request)),
            Ok(vec![source_output_paint.placement_key]),
            "{output_kind:?} drew a released image from the cache"
        );
    }
}

#[test]
fn an_image_budget_that_the_records_fill_keeps_no_encoded_output() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let image_paint = build_placed_image_paint(
        &source_output_paint,
        source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    );
    let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Iterm);
    output_state.set_held_image_byte_count(MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT);
    encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        std::slice::from_ref(&image_paint),
        cell_snapshot.clone(),
        cell_size,
    );
    output_state.prepare_frame(&[], cell_snapshot.clone(), cell_size);
    paint_prepared_frame(&mut output_state);

    output_state.prepare_frame(std::slice::from_ref(&image_paint), cell_snapshot, cell_size);

    assert_eq!(
        worker_request_receiver
            .try_recv()
            .map(|worker_request| list_requested_placement_keys(&worker_request)),
        Ok(vec![source_output_paint.placement_key])
    );
    assert_eq!(output_state.encoded_output_cache.cached_byte_count, 0);
}

#[test]
fn a_kitty_image_that_leaves_and_returns_is_not_compressed_again_until_the_session_releases_it() {
    let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
    let image_content_id = source_output_paint.image_content_id;
    let image_paints = [build_placed_image_paint(
        &source_output_paint,
        source_output_paint.placement_key,
        Rect::new(0, 0, 1, 1),
    )];
    let (mut output_state, worker_request_receiver, worker_message_sender) =
        build_output_state_with_worker_channels(ImageOutputKind::Kitty);
    encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &image_paints,
        None,
        None,
    );
    output_state.prepare_frame(&[], None, None);
    paint_prepared_frame(&mut output_state);

    let (returned_worker_request, _, returned_frame_output_text) = encode_and_paint_frame(
        &mut output_state,
        &worker_request_receiver,
        &worker_message_sender,
        &image_paints,
        None,
        None,
    );

    assert_eq!(
        returned_worker_request.list_requested_kitty_images(),
        [(image_content_id, 2)]
    );
    assert_eq!(
        returned_worker_request.kitty_upload_requests[0]
            .compressed_bytes
            .as_deref(),
        Some(format!("zlib {image_content_id}").as_bytes())
    );
    assert_eq!(
        returned_frame_output_text,
        "upload 2;\x1b[1;1H\x1b_Ga=p,i=2,p=1,x=0,y=0,w=1,h=1,c=1,r=1,C=1,z=0,q=2;\x1b\\\x1b[?25l"
    );

    output_state.prepare_frame(&[], None, None);
    paint_prepared_frame(&mut output_state);
    output_state.release_image_contents(&[image_content_id]);
    output_state.prepare_frame(&image_paints, None, None);

    let released_worker_request = worker_request_receiver
        .try_recv()
        .expect("the released image reaches the worker");
    assert_eq!(
        released_worker_request.list_requested_kitty_images(),
        [(image_content_id, 3)]
    );
    assert_eq!(
        released_worker_request.kitty_upload_requests[0].compressed_bytes,
        None
    );
}

#[test]
fn a_kitty_upload_from_the_kept_payload_equals_the_upload_that_compressed_the_image() {
    let source_output_paint = build_output_paint([10, 20, 30, 255].repeat(6), 3, 2, 0);
    let build_kitty_worker_request = |compressed_bytes: Option<Arc<[u8]>>| WorkerRequest {
        frame_generation: 1,
        output_kind: ImageOutputKind::Kitty,
        pixel_cell_size: PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell"),
        measured_pixel_cell_size: None,
        cell_snapshot: None,
        output_paints: Vec::new(),
        plan_outcomes: Vec::new(),
        encode_keys: Vec::new(),
        kitty_upload_requests: vec![KittyUploadRequest {
            image_content_id: source_output_paint.image_content_id,
            kitty_image_id: 5,
            image: Arc::clone(&source_output_paint.image_record.image),
            compressed_bytes,
        }],
        is_cancellation_requested: Arc::new(AtomicBool::new(false)),
    };
    let run_kitty_upload = |worker_request: &WorkerRequest| {
        let (worker_message_sender, worker_message_receiver) = mpsc::channel();
        run_worker_job(
            worker_request,
            &worker_message_sender,
            MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
        )
        .expect("the worker uploads");
        let worker_messages = worker_message_receiver.try_iter().collect::<Vec<_>>();
        let [WorkerMessage::KittyUploaded {
            finished_kitty_upload,
            compressed_bytes,
            ..
        }] = worker_messages.as_slice()
        else {
            panic!("the worker sends one upload: {worker_messages:?}");
        };
        (
            Arc::clone(&finished_kitty_upload.upload_bytes),
            Arc::clone(compressed_bytes),
        )
    };

    let (compressed_upload_bytes, compressed_bytes) =
        run_kitty_upload(&build_kitty_worker_request(None));
    let (kept_payload_upload_bytes, kept_compressed_bytes) = run_kitty_upload(
        &build_kitty_worker_request(Some(Arc::clone(&compressed_bytes))),
    );

    assert_eq!(kept_payload_upload_bytes, compressed_upload_bytes);
    assert!(Arc::ptr_eq(&kept_compressed_bytes, &compressed_bytes));
}

#[test]
fn a_looping_thirty_frame_animation_encodes_each_frame_once_over_three_loops() {
    for output_kind in list_cell_output_kinds() {
        let placement_key = (PaneId::new(), 1);
        let animation_frame_paints = (0u8..30)
            .map(|frame_index| {
                build_placed_image_paint(
                    &build_output_paint(vec![frame_index, 0, 255 - frame_index, 255], 1, 1, 0),
                    placement_key,
                    Rect::new(0, 0, 1, 1),
                )
            })
            .collect::<Vec<_>>();
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        let mut worker_request_count = 0;
        let mut first_loop_output_texts = Vec::new();

        for shown_frame_index in 0..90 {
            let animation_frame_paint =
                std::slice::from_ref(&animation_frame_paints[shown_frame_index % 30]);
            output_state.prepare_frame(animation_frame_paint, cell_snapshot.clone(), cell_size);
            if let Ok(worker_request) = worker_request_receiver.try_recv() {
                worker_request_count += 1;
                finish_request_with_units(&worker_message_sender, &worker_request);
                output_state.prepare_frame(animation_frame_paint, cell_snapshot.clone(), cell_size);
            }
            let (_, frame_output_text) = paint_prepared_frame(&mut output_state);
            if shown_frame_index < 30 {
                first_loop_output_texts.push(frame_output_text);
            } else {
                assert_eq!(
                    frame_output_text,
                    first_loop_output_texts[shown_frame_index % 30],
                    "{output_kind:?} frame {shown_frame_index}"
                );
            }
        }

        assert_eq!(worker_request_count, 30, "{output_kind:?}");
        assert_eq!(
            first_loop_output_texts[29],
            build_unit_frame_output(output_kind, 0, "unit 30:1"),
            "{output_kind:?}"
        );
    }
}

#[test]
fn a_connection_reset_drops_every_kept_output() {
    for output_kind in list_cell_output_kinds() {
        let source_output_paint = build_output_paint(vec![255, 0, 0, 255], 1, 1, 0);
        let image_paint = build_placed_image_paint(
            &source_output_paint,
            source_output_paint.placement_key,
            Rect::new(0, 0, 1, 1),
        );
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        encode_and_paint_frame(
            &mut output_state,
            &worker_request_receiver,
            &worker_message_sender,
            std::slice::from_ref(&image_paint),
            cell_snapshot.clone(),
            cell_size,
        );

        output_state.reset_connection();
        output_state.prepare_frame(std::slice::from_ref(&image_paint), cell_snapshot, cell_size);

        assert_eq!(
            worker_request_receiver
                .try_recv()
                .map(|worker_request| list_requested_placement_keys(&worker_request)),
            Ok(vec![source_output_paint.placement_key]),
            "{output_kind:?} drew kept output after the reset"
        );
        assert_eq!(
            output_state.encoded_output_cache.cached_byte_count, 0,
            "{output_kind:?}"
        );
    }
}

#[test]
fn a_frame_encodes_only_the_paints_with_no_kept_output() {
    for output_kind in list_cell_output_kinds() {
        let pane_id = PaneId::new();
        let kept_image_paint = build_placed_image_paint(
            &build_output_paint(vec![255, 0, 0, 255], 1, 1, 0),
            (pane_id, 1),
            Rect::new(0, 0, 1, 1),
        );
        let new_image_paint = build_placed_image_paint(
            &build_output_paint(vec![0, 255, 0, 255], 1, 1, 0),
            (pane_id, 2),
            Rect::new(0, 2, 1, 1),
        );
        let cell_snapshot = Some(Arc::new(build_blank_snapshot(Rect::new(0, 0, 8, 8))));
        let cell_size = PixelCellSize::from_pixel_dimensions(1, 1);
        let (mut output_state, worker_request_receiver, worker_message_sender) =
            build_output_state_with_worker_channels(output_kind);
        encode_and_paint_frame(
            &mut output_state,
            &worker_request_receiver,
            &worker_message_sender,
            std::slice::from_ref(&kept_image_paint),
            cell_snapshot.clone(),
            cell_size,
        );

        output_state.prepare_frame(
            &[kept_image_paint, new_image_paint],
            cell_snapshot,
            cell_size,
        );

        let second_worker_request = worker_request_receiver
            .try_recv()
            .expect("the new image reaches the worker");
        assert_eq!(
            second_worker_request
                .output_paints
                .iter()
                .map(|output_paint| output_paint.placement_key)
                .collect::<Vec<_>>(),
            [(pane_id, 1), (pane_id, 2)],
            "{output_kind:?}"
        );
        assert_eq!(
            second_worker_request.encode_keys,
            [
                None,
                Some(output_state.latest_encode_inputs.encode_keys[1].clone())
            ],
            "{output_kind:?}"
        );
    }
}
