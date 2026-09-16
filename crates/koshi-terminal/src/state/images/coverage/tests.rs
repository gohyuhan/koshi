//! Exact native image coverage, movement, and saved-state checks.

use super::*;
use crate::engine::TerminalEngine;
use crate::graphics::{DecodedImage, ImageDimension, ImageDisplay};
use crate::grid::state::RowMetadata;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use koshi_core::process::PtySize;

fn build_terminal_state() -> TerminalState {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 6,
        row_count: 4,
    });
    terminal_state
        .set_cell_size(koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
    terminal_state
}

fn build_image_record(
    protocol: GraphicsProtocol,
    column_count: u16,
    row_count: u16,
    anchor: (u16, u16),
) -> ImageRecord {
    ImageRecord {
        protocol,
        image: Arc::new(DecodedImage {
            pixel_width: u32::from(column_count),
            pixel_height: u32::from(row_count),
            rgba_bytes: (0..row_count)
                .flat_map(|row_index| {
                    (0..column_count).flat_map(move |column_index| {
                        [row_index as u8, column_index as u8, 100, 255]
                    })
                })
                .collect(),
        }),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            requested_width: Some(ImageDimension::Cells(u32::from(column_count))),
            requested_height: Some(ImageDimension::Cells(u32::from(row_count))),
            should_move_cursor: true,
            ..ImageDisplay::default()
        },
        anchor,
    }
}

fn apply_terminal_text(terminal_state: &mut TerminalState, terminal_text: &str) {
    vte::Parser::<{ crate::engine::OSC_BUFFER_BYTE_CAPACITY }>::new_with_size()
        .advance(terminal_state, terminal_text.as_bytes());
}

fn build_iterm_image_command(requested_width_text: &str, requested_height_text: &str) -> Vec<u8> {
    use image::ImageEncoder;

    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(&[255, 0, 0, 255], 1, 1, image::ColorType::Rgba8.into())
        .expect("the one-pixel image encodes");
    format!(
        "\x1b]1337;File=inline=1;width={requested_width_text};height={requested_height_text};preserveAspectRatio=0:{}\x07",
        STANDARD.encode(png_bytes)
    )
    .into_bytes()
}

fn fill_image_storage(terminal_state: &mut TerminalState, storage_byte_count: usize) {
    assert_eq!(storage_byte_count % 4, 0);
    let pixel_count = storage_byte_count / 4;
    let full_row_count = pixel_count / MAX_IMAGE_SIDE_PIXEL_COUNT;
    let remainder_pixel_count = pixel_count % MAX_IMAGE_SIDE_PIXEL_COUNT;
    let mut filler_image_dimensions = Vec::new();
    if full_row_count != 0 {
        filler_image_dimensions.push((MAX_IMAGE_SIDE_PIXEL_COUNT as u32, full_row_count as u32));
    }
    if remainder_pixel_count != 0 {
        filler_image_dimensions.push((1, remainder_pixel_count as u32));
    }
    for (placement_index, (image_pixel_width, image_pixel_height)) in
        filler_image_dimensions.into_iter().enumerate()
    {
        let filler_image_record = ImageRecord {
            protocol: GraphicsProtocol::Kitty,
            image: Arc::new(DecodedImage {
                pixel_width: image_pixel_width,
                pixel_height: image_pixel_height,
                rgba_bytes: vec![0; image_pixel_width as usize * image_pixel_height as usize * 4],
            }),
            animation: None,
            action: ImageAction::TransmitAndDisplay,
            display: ImageDisplay {
                image_id: Some(1_000 + placement_index as u32),
                requested_column_count: Some(1),
                requested_row_count: Some(1),
                should_move_cursor: false,
                ..ImageDisplay::default()
            },
            anchor: (0, placement_index as u16),
        };
        terminal_state
            .apply_image_record(&filler_image_record)
            .expect("the filler fits the remaining storage");
    }
}

type ImagePortion = ((u16, u16), (u16, u16), [u8; 4]);

fn list_image_portions(
    terminal_state: &TerminalState,
    scrollback_offset: usize,
) -> Vec<ImagePortion> {
    terminal_state
        .list_image_placements_for_view(scrollback_offset)
        .iter()
        .flat_map(|image_placement| {
            let image_record = image_placement.create_render_image_record();
            image_placement
                .list_covered_cells()
                .map(move |(row_index, column_index)| {
                    let source_row_index = row_index - image_placement.anchor.0
                        + image_placement.plan.geometry.cell_offset.row;
                    let source_column_index = column_index - image_placement.anchor.1
                        + image_placement.plan.geometry.cell_offset.column;
                    let rgba_start_index = (usize::from(source_row_index)
                        * image_record.image.pixel_width as usize
                        + usize::from(source_column_index))
                        * 4;
                    (
                        (row_index, column_index),
                        (source_row_index, source_column_index),
                        image_record.image.rgba_bytes[rgba_start_index..rgba_start_index + 4]
                            .try_into()
                            .unwrap(),
                    )
                })
        })
        .collect()
}

#[test]
fn native_text_and_erases_remove_only_the_covered_image_portions() {
    for protocol in [GraphicsProtocol::Iterm2, GraphicsProtocol::Sixel] {
        for terminal_text_command in ["\x1b[1;2Hx", "\x1b[1;2Hx\u{0301}", "\x1b[1;2H\x1b[X"] {
            let mut terminal_state = build_terminal_state();
            assert_eq!(
                terminal_state.apply_image_record(&build_image_record(protocol, 3, 1, (0, 0))),
                Ok(())
            );
            assert_eq!(terminal_state.list_image_placements_for_view(0).len(), 1);
            apply_terminal_text(&mut terminal_state, terminal_text_command);
            assert_eq!(
                list_image_portions(&terminal_state, 0),
                [
                    ((0, 0), (0, 0), [0, 0, 100, 255]),
                    ((0, 2), (0, 2), [0, 2, 100, 255])
                ],
                "{protocol:?}: {terminal_text_command:?}"
            );
            let image_placements = terminal_state.list_image_placements_for_view(0);
            assert_eq!(
                image_placements
                    .iter()
                    .map(|image_placement| (
                        image_placement.get_image_anchor(),
                        image_placement.get_image_cell_dimensions(),
                        image_placement.get_image_geometry().full_size
                    ))
                    .collect::<Vec<_>>(),
                [
                    (
                        (0, 0),
                        (1, 1),
                        Size {
                            column_count: 3,
                            row_count: 1
                        }
                    ),
                    (
                        (0, 2),
                        (1, 1),
                        Size {
                            column_count: 3,
                            row_count: 1
                        }
                    )
                ]
            );
            assert_ne!(
                image_placements[0].get_image_placement_id(),
                image_placements[1].get_image_placement_id()
            );
        }
    }
}

#[test]
fn native_wide_text_erases_both_cells_and_line_erase_keeps_other_rows() {
    for protocol in [GraphicsProtocol::Iterm2, GraphicsProtocol::Sixel] {
        let mut terminal_state = build_terminal_state();
        assert_eq!(
            terminal_state.apply_image_record(&build_image_record(protocol, 3, 2, (0, 0))),
            Ok(())
        );
        apply_terminal_text(&mut terminal_state, "\x1b[1;2H界");
        assert_eq!(
            list_image_portions(&terminal_state, 0),
            [
                ((0, 0), (0, 0), [0, 0, 100, 255]),
                ((1, 0), (1, 0), [1, 0, 100, 255]),
                ((1, 1), (1, 1), [1, 1, 100, 255]),
                ((1, 2), (1, 2), [1, 2, 100, 255])
            ]
        );
        apply_terminal_text(&mut terminal_state, "\x1b[2K");
        assert_eq!(
            list_image_portions(&terminal_state, 0),
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
        let mut terminal_state = build_terminal_state();
        assert_eq!(
            terminal_state.apply_image_record(&build_image_record(protocol, 3, 1, (0, 0))),
            Ok(())
        );
        apply_terminal_text(&mut terminal_state, "\x1b[1;2H\x1b[@");
        assert_eq!(
            list_image_portions(&terminal_state, 0),
            [
                ((0, 0), (0, 0), [0, 0, 100, 255]),
                ((0, 2), (0, 1), [0, 1, 100, 255]),
                ((0, 3), (0, 2), [0, 2, 100, 255])
            ]
        );
        apply_terminal_text(&mut terminal_state, "\x1b[P");
        assert_eq!(
            list_image_portions(&terminal_state, 0),
            [
                ((0, 0), (0, 0), [0, 0, 100, 255]),
                ((0, 1), (0, 1), [0, 1, 100, 255]),
                ((0, 2), (0, 2), [0, 2, 100, 255])
            ]
        );
    }
}

#[test]
fn native_partial_line_operations_move_source_coordinates_with_cells() {
    for protocol in [GraphicsProtocol::Iterm2, GraphicsProtocol::Sixel] {
        let mut terminal_state = build_terminal_state();
        assert_eq!(
            terminal_state.apply_image_record(&build_image_record(protocol, 1, 1, (1, 3))),
            Ok(())
        );
        assert_eq!(
            terminal_state.native_fragment_count_by_image_source_id,
            HashMap::from([(1, 1)])
        );

        apply_terminal_text(&mut terminal_state, "\x1b[?69h\x1b[4;5s\x1b[2;1H\x1b[L");

        assert_eq!(
            list_image_portions(&terminal_state, 0),
            [((2, 3), (0, 0), [0, 0, 100, 255])]
        );
        assert_eq!(
            terminal_state.native_fragment_count_by_image_source_id,
            HashMap::from([(1, 1)])
        );

        apply_terminal_text(&mut terminal_state, "\x1b[M");

        assert_eq!(
            list_image_portions(&terminal_state, 0),
            [((1, 3), (0, 0), [0, 0, 100, 255])]
        );
        assert_eq!(
            terminal_state.native_fragment_count_by_image_source_id,
            HashMap::from([(1, 1)])
        );
    }
}

#[test]
fn iterm_rows_scroll_into_history_and_cursor_ends_on_last_image_row() {
    let mut terminal_state = build_terminal_state();
    assert_eq!(
        terminal_state.apply_image_record(&build_image_record(
            GraphicsProtocol::Iterm2,
            2,
            5,
            (2, 1)
        )),
        Ok(())
    );
    assert_eq!(terminal_state.get_active_cursor_position(), (3, 3));
    assert_eq!(terminal_state.scrollback.get_total_pushed_line_count(), 3);
    assert_eq!(
        list_image_portions(&terminal_state, 0),
        (1..5)
            .flat_map(|row_index| (0..2).map(move |column_index| (
                (row_index - 1, column_index + 1),
                (row_index, column_index),
                [row_index as u8, column_index as u8, 100, 255]
            )))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        list_image_portions(&terminal_state, 3),
        (0..2)
            .flat_map(|row_index| (0..2).map(move |column_index| (
                (row_index + 2, column_index + 1),
                (row_index, column_index),
                [row_index as u8, column_index as u8, 100, 255]
            )))
            .collect::<Vec<_>>()
    );
    let restored_terminal_state: TerminalState =
        serde_json::from_value(serde_json::to_value(&terminal_state).unwrap()).unwrap();
    assert_eq!(
        list_image_portions(&restored_terminal_state, 0),
        list_image_portions(&terminal_state, 0)
    );
    assert_eq!(
        list_image_portions(&restored_terminal_state, 3),
        list_image_portions(&terminal_state, 3)
    );
}

#[test]
fn tall_sixel_scrolling_keeps_its_latest_rows_and_retains_history() {
    let mut terminal_state = build_terminal_state();
    let image_record = build_image_record(GraphicsProtocol::Sixel, 1, 10, (0, 0));
    assert_eq!(
        terminal_state.apply_image_record_with_sixel_options(
            &image_record,
            Some(true),
            false,
            None,
        ),
        Ok(())
    );

    assert_eq!(terminal_state.get_active_cursor_position(), (3, 0));
    assert_eq!(terminal_state.scrollback.get_total_pushed_line_count(), 6);
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 6);
    assert_eq!(
        list_image_portions(&terminal_state, 0)
            .iter()
            .map(|(target_cell_position, source_image_position, _)| {
                (*target_cell_position, *source_image_position)
            })
            .collect::<Vec<_>>(),
        [
            ((0, 0), (6, 0)),
            ((1, 0), (7, 0)),
            ((2, 0), (8, 0)),
            ((3, 0), (9, 0))
        ]
    );
    assert_eq!(
        list_image_portions(&terminal_state, 6)
            .iter()
            .map(|(target_cell_position, source_image_position, _)| {
                (*target_cell_position, *source_image_position)
            })
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
    let mut terminal_state = build_terminal_state();
    let image_record = build_image_record(GraphicsProtocol::Sixel, 1, 10, (0, 0));
    assert_eq!(
        terminal_state.apply_image_record_with_sixel_options(
            &image_record,
            Some(false),
            false,
            None,
        ),
        Ok(())
    );

    assert_eq!(terminal_state.get_active_cursor_position(), (3, 0));
    assert_eq!(terminal_state.scrollback.get_total_pushed_line_count(), 0);
    assert_eq!(
        list_image_portions(&terminal_state, 0)
            .iter()
            .map(|(target_cell_position, source_image_position, _)| {
                (*target_cell_position, *source_image_position)
            })
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
    let mut terminal_state = build_terminal_state();
    assert_eq!(
        terminal_state.apply_image_record(&build_image_record(
            GraphicsProtocol::Iterm2,
            4,
            4,
            (0, 5)
        )),
        Ok(())
    );

    let image_placements = terminal_state.list_image_placements_for_view(0);
    assert_eq!(image_placements.len(), 1);
    assert_eq!(image_placements[0].get_image_anchor(), (0, 5));
    assert_eq!(image_placements[0].get_image_cell_dimensions(), (1, 1));
    assert_eq!(
        image_placements[0].get_image_geometry().full_size,
        Size {
            column_count: 1,
            row_count: 1
        }
    );
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 5));
    assert!(terminal_state.active_cursor().pending_wrap);
    assert_eq!(terminal_state.scrollback.get_total_pushed_line_count(), 0);
}

#[test]
fn iterm_height_cap_has_exact_cursor_scrollback_and_source_rows() {
    let mut terminal_state = build_terminal_state();
    assert_eq!(
        terminal_state.apply_image_record(&build_image_record(
            GraphicsProtocol::Iterm2,
            1,
            300,
            (0, 0)
        )),
        Ok(())
    );

    assert_eq!(terminal_state.get_active_cursor_position(), (3, 1));
    assert!(!terminal_state.active_cursor().pending_wrap);
    assert_eq!(terminal_state.scrollback.get_total_pushed_line_count(), 251);
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 251);
    assert_eq!(
        list_image_portions(&terminal_state, 0)
            .iter()
            .map(|(target_cell_position, source_image_position, _)| {
                (*target_cell_position, *source_image_position)
            })
            .collect::<Vec<_>>(),
        [
            ((0, 0), (251, 0)),
            ((1, 0), (252, 0)),
            ((2, 0), (253, 0)),
            ((3, 0), (254, 0))
        ]
    );
    assert_eq!(
        list_image_portions(&terminal_state, 251)
            .iter()
            .map(|(target_cell_position, source_image_position, _)| {
                (*target_cell_position, *source_image_position)
            })
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
    let requested_dimension_texts = ["-2", "0", "-2px", "0px", "-2%", "0%"];
    for requested_dimension_text in requested_dimension_texts {
        for (requested_width_text, requested_height_text) in [
            (requested_dimension_text, "1"),
            ("1", requested_dimension_text),
        ] {
            let mut engine = TerminalEngine::from_pty_size(PtySize {
                column_count: 6,
                row_count: 4,
            });
            engine.set_cell_size(
                koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1)
                    .expect("nonzero cell"),
            );

            assert_eq!(
                engine.process_pty_output(&build_iterm_image_command(
                    requested_width_text,
                    requested_height_text,
                )),
                b""
            );
            let image_placements = engine
                .get_terminal_state()
                .list_image_placements_for_view(0);
            assert_eq!(
                image_placements.len(),
                1,
                "width={requested_width_text}, height={requested_height_text}"
            );
            assert_eq!(
                image_placements[0].get_image_cell_dimensions(),
                (1, 1),
                "width={requested_width_text}, height={requested_height_text}"
            );
            assert_eq!(
                engine.get_terminal_state().get_active_cursor_position(),
                (0, 1),
                "width={requested_width_text}, height={requested_height_text}"
            );
            assert_eq!(
                engine
                    .get_terminal_state()
                    .scrollback
                    .get_total_pushed_line_count(),
                0,
                "width={requested_width_text}, height={requested_height_text}"
            );
        }
    }
}

#[test]
fn native_holes_and_screen_ownership_survive_restore() {
    for protocol in [GraphicsProtocol::Iterm2, GraphicsProtocol::Sixel] {
        let mut terminal_state = build_terminal_state();
        assert_eq!(
            terminal_state.apply_image_record(&build_image_record(protocol, 3, 1, (0, 0))),
            Ok(())
        );
        apply_terminal_text(&mut terminal_state, "\x1b[1;2H\x1b[X");
        let primary_image_portions = list_image_portions(&terminal_state, 0);
        apply_terminal_text(&mut terminal_state, "\x1b[?1049h");
        assert_eq!(list_image_portions(&terminal_state, 0), []);
        assert_eq!(
            terminal_state.apply_image_record(&build_image_record(protocol, 1, 1, (1, 1))),
            Ok(())
        );
        let alternate_image_portions = list_image_portions(&terminal_state, 0);
        let mut restored_terminal_state: TerminalState =
            serde_json::from_value(serde_json::to_value(&terminal_state).unwrap()).unwrap();
        assert_eq!(
            list_image_portions(&restored_terminal_state, 0),
            alternate_image_portions
        );
        apply_terminal_text(&mut restored_terminal_state, "\x1b[?1049l");
        assert_eq!(
            list_image_portions(&restored_terminal_state, 0),
            primary_image_portions
        );
        assert_eq!(
            restored_terminal_state
                .native_images
                .iter()
                .map(|native_image| native_image.screen)
                .collect::<Vec<_>>(),
            [Screen::Primary]
        );
    }
}

#[test]
fn dangling_native_source_reference_is_rejected_on_restore() {
    let mut terminal_state = build_terminal_state();
    assert_eq!(
        terminal_state.apply_image_record(&build_image_record(
            GraphicsProtocol::Sixel,
            1,
            1,
            (0, 0)
        )),
        Ok(())
    );
    let mut serialized_terminal_state = serde_json::to_value(&terminal_state).unwrap();
    serialized_terminal_state["primary"]["rows"][0][0]["combining"]["image_fragments"][0]
        ["source"] = serde_json::json!(u64::MAX);
    assert_eq!(
        serde_json::from_value::<TerminalState>(serialized_terminal_state)
            .unwrap_err()
            .to_string(),
        "native image fragment has no source"
    );
}

#[test]
fn sixel_overlays_keep_both_sources_and_text_clears_both_image_portions() {
    let mut terminal_state = build_terminal_state();
    assert_eq!(
        terminal_state.apply_image_record(&build_image_record(
            GraphicsProtocol::Sixel,
            2,
            1,
            (0, 0)
        )),
        Ok(())
    );
    let mut overlay_image_record = build_image_record(GraphicsProtocol::Sixel, 2, 1, (0, 0));
    overlay_image_record.image = Arc::new(DecodedImage {
        pixel_width: 2,
        pixel_height: 1,
        rgba_bytes: vec![0, 0, 0, 0, 255, 0, 0, 255],
    });
    assert_eq!(
        terminal_state.apply_image_record(&overlay_image_record),
        Ok(())
    );
    assert_eq!(
        list_image_portions(&terminal_state, 0),
        [
            ((0, 0), (0, 0), [0, 0, 100, 255]),
            ((0, 1), (0, 1), [0, 1, 100, 255]),
            ((0, 0), (0, 0), [0, 0, 0, 0]),
            ((0, 1), (0, 1), [255, 0, 0, 255])
        ]
    );
    apply_terminal_text(&mut terminal_state, "\x1b[1;2Hx");
    assert_eq!(
        list_image_portions(&terminal_state, 0),
        [
            ((0, 0), (0, 0), [0, 0, 100, 255]),
            ((0, 0), (0, 0), [0, 0, 0, 0])
        ]
    );
}

#[test]
fn updating_one_sixel_source_preserves_sibling_sources_and_paint_order() {
    let mut cell = Cell::blank();
    let first_image_fragment = ImageCellFragment {
        image_source_id: 11,
        source_row_index: 0,
        source_column_index: 0,
    };
    let second_image_fragment = ImageCellFragment {
        image_source_id: 12,
        source_row_index: 1,
        source_column_index: 1,
    };
    let updated_first_image_fragment = ImageCellFragment {
        image_source_id: 11,
        source_row_index: 2,
        source_column_index: 3,
    };

    cell.set_image_fragment(first_image_fragment, true);
    cell.set_image_fragment(second_image_fragment, true);
    cell.set_image_fragment(updated_first_image_fragment, true);

    assert_eq!(
        cell.image_fragments(),
        [updated_first_image_fragment, second_image_fragment]
    );
}

#[test]
fn native_fragment_storage_counts_each_persistent_allocation() {
    let mut cell = Cell::blank();
    let image_fragment_size_bytes = std::mem::size_of::<ImageCellFragment>();
    let first_image_fragment = ImageCellFragment {
        image_source_id: 11,
        source_row_index: 0,
        source_column_index: 0,
    };
    let second_image_fragment = ImageCellFragment {
        image_source_id: 12,
        source_row_index: 0,
        source_column_index: 0,
    };
    let third_image_fragment = ImageCellFragment {
        image_source_id: 13,
        source_row_index: 0,
        source_column_index: 0,
    };

    assert_eq!(cell.image_fragment_storage_bytes(), 0);
    cell.set_image_fragment(first_image_fragment, true);
    let one_fragment_storage_byte_count = cell.image_fragment_storage_bytes();
    assert!(one_fragment_storage_byte_count > 0);
    cell.set_image_fragment(second_image_fragment, true);
    let two_fragment_storage_byte_count = cell.image_fragment_storage_bytes();
    assert_eq!(
        two_fragment_storage_byte_count - one_fragment_storage_byte_count,
        cell.get_image_fragment_capacity() * image_fragment_size_bytes
    );
    cell.set_image_fragment(third_image_fragment, true);
    let three_fragment_storage_byte_count = cell.image_fragment_storage_bytes();
    assert_eq!(
        three_fragment_storage_byte_count - one_fragment_storage_byte_count,
        cell.get_image_fragment_capacity() * image_fragment_size_bytes
    );
    cell.set_image_fragment(
        ImageCellFragment {
            image_source_id: 12,
            source_row_index: 3,
            source_column_index: 4,
        },
        true,
    );
    assert_eq!(
        cell.image_fragment_storage_bytes(),
        three_fragment_storage_byte_count
    );
}

#[test]
fn native_fragment_overlays_reuse_capacity_and_keep_exact_order() {
    let mut cell = Cell::blank();
    for image_source_id in 1..=MAX_IMAGE_PLACEMENT_COUNT as u64 {
        cell.set_image_fragment(
            ImageCellFragment {
                image_source_id,
                source_row_index: image_source_id as u16,
                source_column_index: image_source_id as u16,
            },
            true,
        );
    }

    assert_eq!(cell.image_fragments().len(), MAX_IMAGE_PLACEMENT_COUNT);
    assert_eq!(
        cell.image_fragments(),
        (1..=MAX_IMAGE_PLACEMENT_COUNT as u64)
            .map(|image_source_id| ImageCellFragment {
                image_source_id,
                source_row_index: image_source_id as u16,
                source_column_index: image_source_id as u16,
            })
            .collect::<Vec<_>>()
    );
    let mut single_fragment_cell = Cell::blank();
    single_fragment_cell.set_image_fragment(cell.image_fragments()[0], true);
    let cell_image_fragment_storage_byte_count = cell.image_fragment_storage_bytes();
    assert_eq!(
        cell_image_fragment_storage_byte_count,
        single_fragment_cell.image_fragment_storage_bytes()
            + cell.get_image_fragment_capacity() * std::mem::size_of::<ImageCellFragment>()
    );

    let restored_cell: Cell =
        serde_json::from_value(serde_json::to_value(&cell).expect("the cell serializes"))
            .expect("the cell restores");
    assert_eq!(restored_cell.image_fragments(), cell.image_fragments());
    assert_eq!(
        restored_cell.image_fragment_storage_bytes(),
        single_fragment_cell.image_fragment_storage_bytes()
            + restored_cell.get_image_fragment_capacity()
                * std::mem::size_of::<ImageCellFragment>()
    );
}

#[test]
fn iterm_replacement_succeeds_when_final_storage_exactly_fits() {
    let mut terminal_state = build_terminal_state();
    let first_image_record = build_image_record(GraphicsProtocol::Iterm2, 1, 1, (0, 0));
    terminal_state
        .apply_image_record(&first_image_record)
        .expect("the first image fits");
    let remaining_image_storage_byte_count =
        MAX_IMAGE_STORAGE_BYTE_COUNT - terminal_state.get_image_storage_byte_count();
    fill_image_storage(&mut terminal_state, remaining_image_storage_byte_count);
    assert_eq!(
        terminal_state.get_image_storage_byte_count(),
        MAX_IMAGE_STORAGE_BYTE_COUNT
    );

    let mut replacement_image_record = first_image_record;
    replacement_image_record.image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![7, 8, 9, 255],
    });
    terminal_state
        .apply_image_record(&replacement_image_record)
        .expect("the replacement keeps the same retained byte count");

    assert_eq!(
        terminal_state.get_image_storage_byte_count(),
        MAX_IMAGE_STORAGE_BYTE_COUNT
    );
    assert_eq!(terminal_state.native_images.len(), 1);
    assert_eq!(
        terminal_state.native_images[0]
            .placement
            .get_image_record()
            .image
            .rgba_bytes,
        [7, 8, 9, 255]
    );
}

#[test]
fn distinct_sixel_overlay_rejects_atomically_when_final_storage_exceeds_the_limit() {
    let mut terminal_state = build_terminal_state();
    let first_image_record = build_image_record(GraphicsProtocol::Sixel, 1, 1, (0, 0));
    terminal_state
        .apply_image_record(&first_image_record)
        .expect("the first image fits");
    let remaining_image_storage_byte_count =
        MAX_IMAGE_STORAGE_BYTE_COUNT - terminal_state.get_image_storage_byte_count();
    fill_image_storage(&mut terminal_state, remaining_image_storage_byte_count);
    assert_eq!(
        terminal_state.get_image_storage_byte_count(),
        MAX_IMAGE_STORAGE_BYTE_COUNT
    );
    let terminal_state_before_second_image = terminal_state.clone();
    let mut second_image_record = first_image_record;
    second_image_record.image = Arc::new(DecodedImage {
        pixel_width: 1,
        pixel_height: 1,
        rgba_bytes: vec![7, 8, 9, 255],
    });
    let requested_storage_byte_count = 4 + 2 * std::mem::size_of::<ImageCellFragment>();

    assert_eq!(
        terminal_state.apply_image_record(&second_image_record),
        Err(ImagePlacementError::StorageLimit {
            used_byte_count: MAX_IMAGE_STORAGE_BYTE_COUNT,
            requested_byte_count: requested_storage_byte_count,
            byte_limit: MAX_IMAGE_STORAGE_BYTE_COUNT,
        })
    );
    assert_eq!(terminal_state, terminal_state_before_second_image);
}

#[test]
fn erasing_the_last_native_fragments_removes_the_canonical_source() {
    let mut terminal_state = build_terminal_state();
    assert_eq!(
        terminal_state.apply_image_record(&build_image_record(
            GraphicsProtocol::Sixel,
            2,
            1,
            (0, 0)
        )),
        Ok(())
    );
    assert_eq!(terminal_state.native_images.len(), 1);
    assert_eq!(
        terminal_state.native_fragment_count_by_image_source_id,
        HashMap::from([(1, 2)])
    );

    apply_terminal_text(&mut terminal_state, "\x1b[1;1Hxx");

    assert_eq!(terminal_state.list_image_placements_for_view(0), []);
    assert_eq!(terminal_state.native_images, []);
    assert_eq!(
        terminal_state.native_fragment_count_by_image_source_id,
        HashMap::new()
    );
    let serialized_terminal_state =
        serde_json::to_value(&terminal_state).expect("terminal state serializes");
    assert_eq!(
        serialized_terminal_state["image_contents"],
        serde_json::json!([])
    );
}

#[test]
fn scrolling_with_one_image_in_long_history_does_not_rebuild_coverage() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 1,
        row_count: 1,
    });
    terminal_state
        .set_cell_size(koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
    let blank_row = [Cell::blank()];
    for _ in 0..5_000 {
        terminal_state
            .scrollback
            .push_row(&blank_row, RowMetadata::default());
    }
    assert_eq!(
        terminal_state.apply_image_record(&build_image_record(
            GraphicsProtocol::Iterm2,
            1,
            1,
            (0, 0)
        )),
        Ok(())
    );
    apply_terminal_text(&mut terminal_state, "\n");
    for _ in 0..4_998 {
        terminal_state
            .scrollback
            .push_row(&blank_row, RowMetadata::default());
    }
    assert_eq!(
        terminal_state.native_fragment_count_by_image_source_id,
        HashMap::from([(1, 1)])
    );

    REBUILD_CELL_VISITS.with(|visits| visits.set(0));
    for _ in 0..128 {
        apply_terminal_text(&mut terminal_state, "\n");
    }

    assert_eq!(REBUILD_CELL_VISITS.with(std::cell::Cell::get), 0);
    assert_eq!(
        terminal_state.native_fragment_count_by_image_source_id,
        HashMap::from([(1, 1)])
    );
    assert_eq!(terminal_state.native_images.len(), 1);

    for _ in 0..4_874 {
        apply_terminal_text(&mut terminal_state, "\n");
    }
    assert_eq!(REBUILD_CELL_VISITS.with(std::cell::Cell::get), 0);
    assert_eq!(
        terminal_state.native_fragment_count_by_image_source_id,
        HashMap::new()
    );
    assert_eq!(terminal_state.native_images, []);
}

#[test]
fn native_sixel_at_the_right_edge_does_not_overflow_cell_coordinates() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: u16::MAX,
        row_count: 1,
    });
    terminal_state
        .set_cell_size(koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
    terminal_state.active_cursor_mut().column = u16::MAX - 1;
    let image_record = build_image_record(GraphicsProtocol::Sixel, 2, 1, (0, u16::MAX - 1));

    assert_eq!(
        terminal_state.apply_image_record_with_sixel_options(
            &image_record,
            Some(true),
            false,
            None,
        ),
        Ok(())
    );
    assert_eq!(
        terminal_state
            .list_image_placements_for_view(0)
            .iter()
            .map(|placement| (
                placement.get_image_anchor(),
                placement.get_image_cell_dimensions(),
                placement.get_image_geometry().cell_offset
            ))
            .collect::<Vec<_>>(),
        [((0, u16::MAX - 1), (1, 1), Point { column: 0, row: 0 })]
    );
}

#[test]
fn native_sixel_outside_a_scroll_region_clips_at_the_bottom_row() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 1,
        row_count: u16::MAX,
    });
    terminal_state
        .set_cell_size(koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
    terminal_state.primary_scroll_region = Some((0, u16::MAX - 2));
    let image_record = build_image_record(GraphicsProtocol::Sixel, 1, 3, (u16::MAX - 1, 0));

    assert_eq!(
        terminal_state.apply_image_record_with_sixel_options(
            &image_record,
            Some(true),
            false,
            None,
        ),
        Ok(())
    );
    let image_placements = terminal_state.list_image_placements_for_view(0);
    assert_eq!(image_placements.len(), 1);
    assert_eq!(image_placements[0].get_image_anchor(), (u16::MAX - 1, 0));
    assert_eq!(image_placements[0].get_image_cell_dimensions(), (1, 1));
    assert_eq!(
        image_placements[0].get_image_geometry().cell_offset,
        Point { column: 0, row: 0 }
    );
}

#[test]
fn checkerboard_coverage_has_five_thousand_distinct_visible_runs() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 100,
        row_count: 100,
    });
    terminal_state
        .set_cell_size(koshi_core::geometry::PixelCellSize::from_pixel_dimensions(1, 1).unwrap());
    assert_eq!(
        terminal_state.apply_image_record(&build_image_record(
            GraphicsProtocol::Sixel,
            100,
            100,
            (0, 0)
        )),
        Ok(())
    );
    let active_grid = terminal_state.active_grid_mut();
    for row_index in 0..100 {
        for column_index in 0..100 {
            if (row_index + column_index) % 2 == 0 {
                *active_grid.get_cell_mut(row_index, column_index).unwrap() = Cell::blank();
            }
        }
    }
    let image_placements = terminal_state.list_image_placements_for_view(0);
    let expected_visible_image_portions = (0..100)
        .flat_map(|row_index| {
            (0..100).filter_map(move |column_index| {
                ((row_index + column_index) % 2 == 1).then_some((
                    (row_index, column_index),
                    (1, 1),
                    Point {
                        column: column_index,
                        row: row_index,
                    },
                ))
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        image_placements
            .iter()
            .map(|image_placement| (
                image_placement.get_image_anchor(),
                image_placement.get_image_cell_dimensions(),
                image_placement.get_image_geometry().cell_offset
            ))
            .collect::<Vec<_>>(),
        expected_visible_image_portions
    );
    assert_eq!(
        image_placements
            .iter()
            .map(ImagePlacement::get_image_placement_id)
            .collect::<Vec<_>>(),
        (1..=5000).collect::<Vec<_>>()
    );
}
