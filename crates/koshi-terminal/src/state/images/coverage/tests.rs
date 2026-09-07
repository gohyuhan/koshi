//! Exact native image coverage, movement, and saved-state checks.

use super::*;
use crate::engine::TerminalEngine;
use crate::graphics::{DecodedImage, ImageDimension, ImageDisplay};
use crate::grid::state::RowMeta;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use koshi_core::process::PtySize;

fn state() -> TerminalState {
    let mut state = TerminalState::new(PtySize { cols: 6, rows: 4 });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).unwrap());
    state
}

fn record(protocol: GraphicsProtocol, columns: u16, rows: u16, anchor: (u16, u16)) -> ImageRecord {
    ImageRecord {
        protocol,
        image: Arc::new(DecodedImage {
            width: u32::from(columns),
            height: u32::from(rows),
            rgba: (0..rows)
                .flat_map(|row| {
                    (0..columns).flat_map(move |column| [row as u8, column as u8, 100, 255])
                })
                .collect(),
        }),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            width: Some(ImageDimension::Cells(u32::from(columns))),
            height: Some(ImageDimension::Cells(u32::from(rows))),
            move_cursor: true,
            ..ImageDisplay::default()
        },
        anchor,
    }
}

fn write(state: &mut TerminalState, text: &str) {
    vte::Parser::<{ crate::engine::OSC_CAPACITY }>::new_with_size().advance(state, text.as_bytes());
}

fn iterm_image(width: &str, height: &str) -> Vec<u8> {
    use image::ImageEncoder;

    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the one-pixel image encodes");
    format!(
        "\x1b]1337;File=inline=1;width={width};height={height};preserveAspectRatio=0:{}\x07",
        STANDARD.encode(png)
    )
    .into_bytes()
}

fn fill_image_storage(state: &mut TerminalState, bytes: usize) {
    assert_eq!(bytes % 4, 0);
    let pixels = bytes / 4;
    let full_rows = pixels / MAX_IMAGE_SIDE;
    let remainder = pixels % MAX_IMAGE_SIDE;
    let mut dimensions = Vec::new();
    if full_rows != 0 {
        dimensions.push((MAX_IMAGE_SIDE as u32, full_rows as u32));
    }
    if remainder != 0 {
        dimensions.push((1, remainder as u32));
    }
    for (index, (width, height)) in dimensions.into_iter().enumerate() {
        let filler = ImageRecord {
            protocol: GraphicsProtocol::Kitty,
            image: Arc::new(DecodedImage {
                width,
                height,
                rgba: vec![0; width as usize * height as usize * 4],
            }),
            animation: None,
            action: ImageAction::TransmitAndDisplay,
            display: ImageDisplay {
                image_id: Some(1_000 + index as u32),
                cell_columns: Some(1),
                cell_rows: Some(1),
                move_cursor: false,
                ..ImageDisplay::default()
            },
            anchor: (0, index as u16),
        };
        state
            .apply_image_record(&filler)
            .expect("the filler fits the remaining storage");
    }
}

type ImagePortion = ((u16, u16), (u16, u16), [u8; 4]);

fn portions(state: &TerminalState, offset: usize) -> Vec<ImagePortion> {
    state
        .image_placements_for_view(offset)
        .iter()
        .flat_map(|placement| {
            let record = placement.render_record_arc();
            placement.covered_cells().map(move |(row, column)| {
                let source_row = row - placement.anchor.0 + placement.plan.geometry.offset.y;
                let source_column = column - placement.anchor.1 + placement.plan.geometry.offset.x;
                let index = (usize::from(source_row) * record.image.width as usize
                    + usize::from(source_column))
                    * 4;
                (
                    (row, column),
                    (source_row, source_column),
                    record.image.rgba[index..index + 4].try_into().unwrap(),
                )
            })
        })
        .collect()
}

#[test]
fn native_text_and_erases_remove_only_the_covered_cell_portions() {
    for protocol in [GraphicsProtocol::Iterm2, GraphicsProtocol::Sixel] {
        for command in ["\x1b[1;2Hx", "\x1b[1;2Hx\u{0301}", "\x1b[1;2H\x1b[X"] {
            let mut state = state();
            assert_eq!(
                state.apply_image_record(&record(protocol, 3, 1, (0, 0))),
                Ok(())
            );
            assert_eq!(state.image_placements_for_view(0).len(), 1);
            write(&mut state, command);
            assert_eq!(
                portions(&state, 0),
                [
                    ((0, 0), (0, 0), [0, 0, 100, 255]),
                    ((0, 2), (0, 2), [0, 2, 100, 255])
                ],
                "{protocol:?}: {command:?}"
            );
            let placements = state.image_placements_for_view(0);
            assert_eq!(
                placements
                    .iter()
                    .map(|p| (p.anchor(), p.dimensions(), p.geometry().full_size))
                    .collect::<Vec<_>>(),
                [
                    ((0, 0), (1, 1), Size { cols: 3, rows: 1 }),
                    ((0, 2), (1, 1), Size { cols: 3, rows: 1 })
                ]
            );
            assert_ne!(placements[0].id(), placements[1].id());
        }
    }
}

#[test]
fn native_wide_text_erases_both_cells_and_line_erase_keeps_other_rows() {
    for protocol in [GraphicsProtocol::Iterm2, GraphicsProtocol::Sixel] {
        let mut state = state();
        assert_eq!(
            state.apply_image_record(&record(protocol, 3, 2, (0, 0))),
            Ok(())
        );
        write(&mut state, "\x1b[1;2H界");
        assert_eq!(
            portions(&state, 0),
            [
                ((0, 0), (0, 0), [0, 0, 100, 255]),
                ((1, 0), (1, 0), [1, 0, 100, 255]),
                ((1, 1), (1, 1), [1, 1, 100, 255]),
                ((1, 2), (1, 2), [1, 2, 100, 255])
            ]
        );
        write(&mut state, "\x1b[2K");
        assert_eq!(
            portions(&state, 0),
            [
                ((1, 0), (1, 0), [1, 0, 100, 255]),
                ((1, 1), (1, 1), [1, 1, 100, 255]),
                ((1, 2), (1, 2), [1, 2, 100, 255])
            ]
        );
    }
}

#[test]
fn native_insert_delete_cells_move_the_source_coordinates_with_cells() {
    for protocol in [GraphicsProtocol::Iterm2, GraphicsProtocol::Sixel] {
        let mut state = state();
        assert_eq!(
            state.apply_image_record(&record(protocol, 3, 1, (0, 0))),
            Ok(())
        );
        write(&mut state, "\x1b[1;2H\x1b[@");
        assert_eq!(
            portions(&state, 0),
            [
                ((0, 0), (0, 0), [0, 0, 100, 255]),
                ((0, 2), (0, 1), [0, 1, 100, 255]),
                ((0, 3), (0, 2), [0, 2, 100, 255])
            ]
        );
        write(&mut state, "\x1b[P");
        assert_eq!(
            portions(&state, 0),
            [
                ((0, 0), (0, 0), [0, 0, 100, 255]),
                ((0, 1), (0, 1), [0, 1, 100, 255]),
                ((0, 2), (0, 2), [0, 2, 100, 255])
            ]
        );
    }
}

#[test]
fn iterm_rows_scroll_into_history_and_cursor_ends_on_last_image_row() {
    let mut state = state();
    assert_eq!(
        state.apply_image_record(&record(GraphicsProtocol::Iterm2, 2, 5, (2, 1))),
        Ok(())
    );
    assert_eq!(state.active_cursor_position(), (3, 3));
    assert_eq!(state.scrollback.total_pushed(), 3);
    assert_eq!(
        portions(&state, 0),
        (1..5)
            .flat_map(|row| (0..2).map(move |column| (
                (row - 1, column + 1),
                (row, column),
                [row as u8, column as u8, 100, 255]
            )))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        portions(&state, 3),
        (0..2)
            .flat_map(|row| (0..2).map(move |column| (
                (row + 2, column + 1),
                (row, column),
                [row as u8, column as u8, 100, 255]
            )))
            .collect::<Vec<_>>()
    );
    let restored: TerminalState =
        serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(portions(&restored, 0), portions(&state, 0));
    assert_eq!(portions(&restored, 3), portions(&state, 3));
}

#[test]
fn tall_sixel_scrolling_keeps_its_latest_rows_and_retains_history() {
    let mut state = state();
    let image = record(GraphicsProtocol::Sixel, 1, 10, (0, 0));
    assert_eq!(
        state.apply_image_record_with_sixel_options(&image, Some(true), false, None),
        Ok(())
    );

    assert_eq!(state.active_cursor_position(), (3, 0));
    assert_eq!(state.scrollback.total_pushed(), 6);
    assert_eq!(state.scrollback.len(), 6);
    assert_eq!(
        portions(&state, 0)
            .iter()
            .map(|(target, source, _)| (*target, *source))
            .collect::<Vec<_>>(),
        [
            ((0, 0), (6, 0)),
            ((1, 0), (7, 0)),
            ((2, 0), (8, 0)),
            ((3, 0), (9, 0))
        ]
    );
    assert_eq!(
        portions(&state, 6)
            .iter()
            .map(|(target, source, _)| (*target, *source))
            .collect::<Vec<_>>(),
        [
            ((0, 0), (0, 0)),
            ((1, 0), (1, 0)),
            ((2, 0), (2, 0)),
            ((3, 0), (3, 0))
        ]
    );
}

#[test]
fn tall_sixel_fixed_graphics_mode_clips_without_scrolling() {
    let mut state = state();
    let image = record(GraphicsProtocol::Sixel, 1, 10, (0, 0));
    assert_eq!(
        state.apply_image_record_with_sixel_options(&image, Some(false), false, None),
        Ok(())
    );

    assert_eq!(state.active_cursor_position(), (3, 0));
    assert_eq!(state.scrollback.total_pushed(), 0);
    assert_eq!(
        portions(&state, 0)
            .iter()
            .map(|(target, source, _)| (*target, *source))
            .collect::<Vec<_>>(),
        [
            ((0, 0), (0, 0)),
            ((1, 0), (1, 0)),
            ((2, 0), (2, 0)),
            ((3, 0), (3, 0))
        ]
    );
}

#[test]
fn iterm_width_is_scaled_to_the_cells_right_of_the_cursor() {
    let mut state = state();
    assert_eq!(
        state.apply_image_record(&record(GraphicsProtocol::Iterm2, 4, 4, (0, 5))),
        Ok(())
    );

    let placements = state.image_placements_for_view(0);
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].anchor(), (0, 5));
    assert_eq!(placements[0].dimensions(), (1, 1));
    assert_eq!(
        placements[0].geometry().full_size,
        Size { cols: 1, rows: 1 }
    );
    assert_eq!(state.active_cursor_position(), (0, 5));
    assert!(state.active_cursor().pending_wrap);
    assert_eq!(state.scrollback.total_pushed(), 0);
}

#[test]
fn iterm_height_cap_has_exact_cursor_scrollback_and_source_rows() {
    let mut state = state();
    assert_eq!(
        state.apply_image_record(&record(GraphicsProtocol::Iterm2, 1, 300, (0, 0))),
        Ok(())
    );

    assert_eq!(state.active_cursor_position(), (3, 1));
    assert!(!state.active_cursor().pending_wrap);
    assert_eq!(state.scrollback.total_pushed(), 251);
    assert_eq!(state.scrollback.len(), 251);
    assert_eq!(
        portions(&state, 0)
            .iter()
            .map(|(target, source, _)| (*target, *source))
            .collect::<Vec<_>>(),
        [
            ((0, 0), (251, 0)),
            ((1, 0), (252, 0)),
            ((2, 0), (253, 0)),
            ((3, 0), (254, 0))
        ]
    );
    assert_eq!(
        portions(&state, 251)
            .iter()
            .map(|(target, source, _)| (*target, *source))
            .collect::<Vec<_>>(),
        [
            ((0, 0), (0, 0)),
            ((1, 0), (1, 0)),
            ((2, 0), (2, 0)),
            ((3, 0), (3, 0))
        ]
    );
}

#[test]
fn nonpositive_iterm_dimensions_render_one_cell() {
    let dimensions = ["-2", "0", "-2px", "0px", "-2%", "0%"];
    for dimension in dimensions {
        for (width, height) in [(dimension, "1"), ("1", dimension)] {
            let mut engine = TerminalEngine::new(PtySize { cols: 6, rows: 4 });
            engine.set_cell_size(
                koshi_core::geometry::PixelCellSize::new(1, 1).expect("nonzero cell"),
            );

            assert_eq!(engine.advance(&iterm_image(width, height)), b"");
            let placements = engine.state().image_placements_for_view(0);
            assert_eq!(placements.len(), 1, "width={width}, height={height}");
            assert_eq!(
                placements[0].dimensions(),
                (1, 1),
                "width={width}, height={height}"
            );
            assert_eq!(
                engine.state().active_cursor_position(),
                (0, 1),
                "width={width}, height={height}"
            );
            assert_eq!(
                engine.state().scrollback.total_pushed(),
                0,
                "width={width}, height={height}"
            );
        }
    }
}

#[test]
fn native_holes_and_screen_ownership_survive_restore() {
    for protocol in [GraphicsProtocol::Iterm2, GraphicsProtocol::Sixel] {
        let mut state = state();
        assert_eq!(
            state.apply_image_record(&record(protocol, 3, 1, (0, 0))),
            Ok(())
        );
        write(&mut state, "\x1b[1;2H\x1b[X");
        let primary = portions(&state, 0);
        write(&mut state, "\x1b[?1049h");
        assert_eq!(portions(&state, 0), []);
        assert_eq!(
            state.apply_image_record(&record(protocol, 1, 1, (1, 1))),
            Ok(())
        );
        let alternate = portions(&state, 0);
        let mut restored: TerminalState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert_eq!(portions(&restored, 0), alternate);
        write(&mut restored, "\x1b[?1049l");
        assert_eq!(portions(&restored, 0), primary);
        assert_eq!(
            restored
                .native_images
                .iter()
                .map(|source| source.screen)
                .collect::<Vec<_>>(),
            [Screen::Primary]
        );
    }
}

#[test]
fn dangling_native_source_reference_is_rejected_on_restore() {
    let mut state = state();
    assert_eq!(
        state.apply_image_record(&record(GraphicsProtocol::Sixel, 1, 1, (0, 0))),
        Ok(())
    );
    let mut value = serde_json::to_value(&state).unwrap();
    value["primary"]["rows"][0][0]["combining"]["image_fragments"][0]["source"] =
        serde_json::json!(u64::MAX);
    assert_eq!(
        serde_json::from_value::<TerminalState>(value)
            .unwrap_err()
            .to_string(),
        "native image fragment has no source"
    );
}

#[test]
fn sixel_overlays_keep_both_sources_and_text_clears_both_portions() {
    let mut state = state();
    assert_eq!(
        state.apply_image_record(&record(GraphicsProtocol::Sixel, 2, 1, (0, 0))),
        Ok(())
    );
    let mut overlay = record(GraphicsProtocol::Sixel, 2, 1, (0, 0));
    overlay.image = Arc::new(DecodedImage {
        width: 2,
        height: 1,
        rgba: vec![0, 0, 0, 0, 255, 0, 0, 255],
    });
    assert_eq!(state.apply_image_record(&overlay), Ok(()));
    assert_eq!(
        portions(&state, 0),
        [
            ((0, 0), (0, 0), [0, 0, 100, 255]),
            ((0, 1), (0, 1), [0, 1, 100, 255]),
            ((0, 0), (0, 0), [0, 0, 0, 0]),
            ((0, 1), (0, 1), [255, 0, 0, 255])
        ]
    );
    write(&mut state, "\x1b[1;2Hx");
    assert_eq!(
        portions(&state, 0),
        [
            ((0, 0), (0, 0), [0, 0, 100, 255]),
            ((0, 0), (0, 0), [0, 0, 0, 0])
        ]
    );
}

#[test]
fn updating_one_sixel_source_preserves_sibling_sources_and_paint_order() {
    let mut cell = Cell::blank();
    let first = ImageCellFragment {
        source: 11,
        row: 0,
        column: 0,
    };
    let second = ImageCellFragment {
        source: 12,
        row: 1,
        column: 1,
    };
    let updated_first = ImageCellFragment {
        source: 11,
        row: 2,
        column: 3,
    };

    cell.set_image_fragment(first, true);
    cell.set_image_fragment(second, true);
    cell.set_image_fragment(updated_first, true);

    assert_eq!(cell.image_fragments(), [updated_first, second]);
}

#[test]
fn native_fragment_storage_counts_each_persistent_allocation() {
    let mut cell = Cell::blank();
    let fragment_size = std::mem::size_of::<ImageCellFragment>();
    let first = ImageCellFragment {
        source: 11,
        row: 0,
        column: 0,
    };
    let second = ImageCellFragment {
        source: 12,
        row: 0,
        column: 0,
    };
    let third = ImageCellFragment {
        source: 13,
        row: 0,
        column: 0,
    };

    assert_eq!(cell.image_fragment_storage_bytes(), 0);
    cell.set_image_fragment(first, true);
    let one = cell.image_fragment_storage_bytes();
    assert!(one > 0);
    cell.set_image_fragment(second, true);
    let two = cell.image_fragment_storage_bytes();
    assert_eq!(two - one, cell.image_fragment_capacity() * fragment_size);
    cell.set_image_fragment(third, true);
    let three = cell.image_fragment_storage_bytes();
    assert_eq!(three - one, cell.image_fragment_capacity() * fragment_size);
    cell.set_image_fragment(
        ImageCellFragment {
            source: 12,
            row: 3,
            column: 4,
        },
        true,
    );
    assert_eq!(cell.image_fragment_storage_bytes(), three);
}

#[test]
fn native_fragment_overlays_reuse_capacity_and_keep_exact_order() {
    let mut cell = Cell::blank();
    for source in 1..=MAX_IMAGE_PLACEMENTS as u64 {
        cell.set_image_fragment(
            ImageCellFragment {
                source,
                row: source as u16,
                column: source as u16,
            },
            true,
        );
    }

    assert_eq!(cell.image_fragments().len(), MAX_IMAGE_PLACEMENTS);
    assert_eq!(
        cell.image_fragments(),
        (1..=MAX_IMAGE_PLACEMENTS as u64)
            .map(|source| ImageCellFragment {
                source,
                row: source as u16,
                column: source as u16,
            })
            .collect::<Vec<_>>()
    );
    let mut one = Cell::blank();
    one.set_image_fragment(cell.image_fragments()[0], true);
    let storage = cell.image_fragment_storage_bytes();
    assert_eq!(
        storage,
        one.image_fragment_storage_bytes()
            + cell.image_fragment_capacity() * std::mem::size_of::<ImageCellFragment>()
    );

    let restored: Cell =
        serde_json::from_value(serde_json::to_value(&cell).expect("the cell serializes"))
            .expect("the cell restores");
    assert_eq!(restored.image_fragments(), cell.image_fragments());
    assert_eq!(
        restored.image_fragment_storage_bytes(),
        one.image_fragment_storage_bytes()
            + restored.image_fragment_capacity() * std::mem::size_of::<ImageCellFragment>()
    );
}

#[test]
fn iterm_replacement_succeeds_when_final_storage_exactly_fits() {
    let mut state = state();
    let first = record(GraphicsProtocol::Iterm2, 1, 1, (0, 0));
    state
        .apply_image_record(&first)
        .expect("the first image fits");
    let remaining = MAX_IMAGE_STORAGE_BYTES - state.image_storage_bytes();
    fill_image_storage(&mut state, remaining);
    assert_eq!(state.image_storage_bytes(), MAX_IMAGE_STORAGE_BYTES);

    let mut replacement = first;
    replacement.image = Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![7, 8, 9, 255],
    });
    state
        .apply_image_record(&replacement)
        .expect("the replacement keeps the same retained byte count");

    assert_eq!(state.image_storage_bytes(), MAX_IMAGE_STORAGE_BYTES);
    assert_eq!(state.native_images.len(), 1);
    assert_eq!(
        state.native_images[0].placement.record().image.rgba,
        [7, 8, 9, 255]
    );
}

#[test]
fn distinct_sixel_overlay_rejects_atomically_when_final_storage_exceeds_the_limit() {
    let mut state = state();
    let first = record(GraphicsProtocol::Sixel, 1, 1, (0, 0));
    state
        .apply_image_record(&first)
        .expect("the first image fits");
    let remaining = MAX_IMAGE_STORAGE_BYTES - state.image_storage_bytes();
    fill_image_storage(&mut state, remaining);
    assert_eq!(state.image_storage_bytes(), MAX_IMAGE_STORAGE_BYTES);
    let before = state.clone();
    let mut second = first;
    second.image = Arc::new(DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![7, 8, 9, 255],
    });
    let requested_bytes = 4 + 2 * std::mem::size_of::<ImageCellFragment>();

    assert_eq!(
        state.apply_image_record(&second),
        Err(ImagePlacementError::StorageLimit {
            used_bytes: MAX_IMAGE_STORAGE_BYTES,
            requested_bytes,
            limit_bytes: MAX_IMAGE_STORAGE_BYTES,
        })
    );
    assert_eq!(state, before);
}

#[test]
fn erasing_the_last_native_fragments_removes_the_canonical_source() {
    let mut state = state();
    assert_eq!(
        state.apply_image_record(&record(GraphicsProtocol::Sixel, 2, 1, (0, 0))),
        Ok(())
    );
    assert_eq!(state.native_images.len(), 1);
    assert_eq!(state.native_fragment_counts, HashMap::from([(1, 2)]));

    write(&mut state, "\x1b[1;1Hxx");

    assert_eq!(state.image_placements_for_view(0), []);
    assert_eq!(state.native_images, []);
    assert_eq!(state.native_fragment_counts, HashMap::new());
    let serialized = serde_json::to_value(&state).expect("terminal state serializes");
    assert_eq!(serialized["image_contents"], serde_json::json!([]));
}

#[test]
fn scrolling_with_one_image_in_long_history_does_not_rebuild_coverage() {
    let mut state = TerminalState::new(PtySize { cols: 1, rows: 1 });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).unwrap());
    let blank = [Cell::blank()];
    for _ in 0..5_000 {
        state.scrollback.push_row(&blank, RowMeta::default());
    }
    assert_eq!(
        state.apply_image_record(&record(GraphicsProtocol::Iterm2, 1, 1, (0, 0))),
        Ok(())
    );
    write(&mut state, "\n");
    for _ in 0..4_998 {
        state.scrollback.push_row(&blank, RowMeta::default());
    }
    assert_eq!(state.native_fragment_counts, HashMap::from([(1, 1)]));

    REBUILD_CELL_VISITS.with(|visits| visits.set(0));
    for _ in 0..128 {
        write(&mut state, "\n");
    }

    assert_eq!(REBUILD_CELL_VISITS.with(std::cell::Cell::get), 0);
    assert_eq!(state.native_fragment_counts, HashMap::from([(1, 1)]));
    assert_eq!(state.native_images.len(), 1);

    for _ in 0..4_874 {
        write(&mut state, "\n");
    }
    assert_eq!(REBUILD_CELL_VISITS.with(std::cell::Cell::get), 0);
    assert_eq!(state.native_fragment_counts, HashMap::new());
    assert_eq!(state.native_images, []);
}

#[test]
fn native_sixel_at_the_right_edge_does_not_overflow_cell_coordinates() {
    let mut state = TerminalState::new(PtySize {
        cols: u16::MAX,
        rows: 1,
    });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).unwrap());
    state.active_cursor_mut().col = u16::MAX - 1;
    let image = record(GraphicsProtocol::Sixel, 2, 1, (0, u16::MAX - 1));

    assert_eq!(
        state.apply_image_record_with_sixel_options(&image, Some(true), false, None),
        Ok(())
    );
    assert_eq!(
        state
            .image_placements_for_view(0)
            .iter()
            .map(|placement| (
                placement.anchor(),
                placement.dimensions(),
                placement.geometry().offset
            ))
            .collect::<Vec<_>>(),
        [((0, u16::MAX - 1), (1, 1), Point { x: 0, y: 0 })]
    );
}

#[test]
fn native_sixel_outside_a_scroll_region_clips_at_the_bottom_row() {
    let mut state = TerminalState::new(PtySize {
        cols: 1,
        rows: u16::MAX,
    });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).unwrap());
    state.primary_scroll_region = Some((0, u16::MAX - 2));
    let image = record(GraphicsProtocol::Sixel, 1, 3, (u16::MAX - 1, 0));

    assert_eq!(
        state.apply_image_record_with_sixel_options(&image, Some(true), false, None),
        Ok(())
    );
    let placements = state.image_placements_for_view(0);
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].anchor(), (u16::MAX - 1, 0));
    assert_eq!(placements[0].dimensions(), (1, 1));
    assert_eq!(placements[0].geometry().offset, Point { x: 0, y: 0 });
}

#[test]
fn checkerboard_coverage_has_five_thousand_distinct_visible_runs() {
    let mut state = TerminalState::new(PtySize {
        cols: 100,
        rows: 100,
    });
    state.set_cell_size(koshi_core::geometry::PixelCellSize::new(1, 1).unwrap());
    assert_eq!(
        state.apply_image_record(&record(GraphicsProtocol::Sixel, 100, 100, (0, 0))),
        Ok(())
    );
    let grid = state.active_grid_mut();
    for row in 0..100 {
        for column in 0..100 {
            if (row + column) % 2 == 0 {
                *grid.cell_mut(row, column).unwrap() = Cell::blank();
            }
        }
    }
    let placements = state.image_placements_for_view(0);
    let expected = (0..100)
        .flat_map(|row| {
            (0..100).filter_map(move |column| {
                ((row + column) % 2 == 1).then_some((
                    (row, column),
                    (1, 1),
                    Point { x: column, y: row },
                ))
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        placements
            .iter()
            .map(|p| (p.anchor(), p.dimensions(), p.geometry().offset))
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        placements
            .iter()
            .map(ImagePlacement::id)
            .collect::<Vec<_>>(),
        (1..=5000).collect::<Vec<_>>()
    );
}
