//! Contract tests for incremental indexed Sixel parsing.

use super::*;

impl IndexedImage {
    /// The normalized `(vertical, horizontal)` pixel aspect ratio.
    fn get_pixel_aspect_ratio(&self) -> (u32, u32) {
        (self.pixel_aspect_vertical, self.pixel_aspect_horizontal)
    }
}

fn parse_sixel_payload(sixel_payload_bytes: &[u8]) -> SixelGraphic {
    parse_sixel_result(sixel_payload_bytes).expect("Sixel payload is valid")
}

fn parse_sixel_result(sixel_payload_bytes: &[u8]) -> Result<SixelGraphic, GraphicsError> {
    let mut sixel_parser = SixelParser::new();
    for &sixel_payload_byte in sixel_payload_bytes {
        sixel_parser.feed_input_byte(sixel_payload_byte)?;
    }
    sixel_parser.finish_payload()
}

fn resolve_sixel_payload(
    sixel_payload_bytes: &[u8],
    terminal_background_rgb: [u8; 3],
) -> DecodedImage {
    let sixel_graphic = parse_sixel_payload(sixel_payload_bytes);
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("payload has an indexed image");
    let mut sixel_palette = SixelPalette::default();
    sixel_palette.apply_palette_changes(sixel_graphic.get_palette_changes());
    indexed_image
        .resolve_indexed_image(&sixel_palette, terminal_background_rgb)
        .expect("image resolves")
}

fn assert_sixel_invalid_command_error(sixel_graphics_error: GraphicsError) {
    match sixel_graphics_error {
        GraphicsError::InvalidCommand { protocol } => {
            assert_eq!(protocol, GraphicsProtocol::Sixel);
        }
        unexpected_sixel_graphics_error => {
            panic!("unexpected error: {unexpected_sixel_graphics_error:?}")
        }
    }
}

fn assert_sixel_image_too_large_error(sixel_graphics_error: GraphicsError) {
    match sixel_graphics_error {
        GraphicsError::ImageTooLarge { protocol } => {
            assert_eq!(protocol, GraphicsProtocol::Sixel);
        }
        unexpected_sixel_graphics_error => {
            panic!("unexpected error: {unexpected_sixel_graphics_error:?}")
        }
    }
}

#[test]
fn sixel_parser_preserves_register_indices_and_terminal_background_rows() {
    let sixel_graphic = parse_sixel_payload(b"q#1;2;100;0;0#1@");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("data column has an extent");

    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (1, 6)
    );
    assert_eq!(indexed_image.get_pixel_aspect_ratio(), (2, 1));
    assert_eq!(
        indexed_image.pixel_register_indices,
        [
            1,
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_TERMINAL_BACKGROUND
        ]
    );
    assert_eq!(
        sixel_graphic.get_sixel_background(),
        SixelBackground::Terminal
    );

    let resolved_image = resolve_sixel_payload(b"q#1;2;100;0;0#1@", [9, 8, 7]);
    assert_eq!(resolved_image.pixel_width, 1);
    assert_eq!(resolved_image.pixel_height, 12);
    assert_eq!(&resolved_image.rgba_bytes[..4], [255, 0, 0, 255]);
    assert_eq!(&resolved_image.rgba_bytes[4..8], [255, 0, 0, 255]);
    assert_eq!(&resolved_image.rgba_bytes[8..12], [9, 8, 7, 255]);
}

#[test]
fn sixel_preserve_mode_uses_set_bit_extent_and_keeps_gaps_transparent() {
    let sixel_graphic = parse_sixel_payload(b"1;1q@-@");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("set bits have an extent");

    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (1, 7)
    );
    assert_eq!(indexed_image.pixel_register_indices[0], 0);
    assert_eq!(
        &indexed_image.pixel_register_indices[1..6],
        [SIXEL_UNTOUCHED; 5]
    );
    assert_eq!(indexed_image.pixel_register_indices[6], 0);

    let resolved_image = resolve_sixel_payload(b"7;1q@-@", [9, 8, 7]);
    assert_eq!(resolved_image.pixel_height, 7);
    assert_eq!(
        &resolved_image.rgba_bytes[4..24],
        [0, 0, 0, 0].repeat(5).as_slice()
    );
}

#[test]
fn sixel_preserve_mode_keeps_declared_zero_bits_unpainted() {
    let sixel_graphic = parse_sixel_payload(b"7;1q\"1;1;2;2?");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("declared dimensions provide an extent");

    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (2, 2)
    );
    assert_eq!(indexed_image.pixel_register_indices, [SIXEL_UNTOUCHED; 4]);
}

#[test]
fn sixel_macro_aspects_match_the_dec_table() {
    let expected_macro_aspects = [
        (0, (2, 1)),
        (1, (2, 1)),
        (2, (5, 1)),
        (3, (3, 1)),
        (4, (3, 1)),
        (5, (2, 1)),
        (6, (2, 1)),
        (7, (1, 1)),
        (8, (1, 1)),
        (9, (1, 1)),
    ];
    for (macro_parameter_value, expected_pixel_aspect_ratio) in expected_macro_aspects {
        let sixel_payload_text = format!("{macro_parameter_value};1q@");
        let sixel_graphic = parse_sixel_payload(sixel_payload_text.as_bytes());
        assert_eq!(
            sixel_graphic
                .get_indexed_image()
                .expect("set bit has an extent")
                .get_pixel_aspect_ratio(),
            expected_pixel_aspect_ratio
        );
    }
    let omitted_macro_parameter_graphic = parse_sixel_payload(b"q@");
    assert_eq!(
        omitted_macro_parameter_graphic
            .get_indexed_image()
            .expect("set bit has an extent")
            .get_pixel_aspect_ratio(),
        (2, 1)
    );
}

#[test]
fn sixel_raster_aspect_is_reduced_and_overrides_macro_aspect() {
    let sixel_graphic = parse_sixel_payload(b"2;1q\"6;4;1;1@");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("set bit has an extent");
    assert_eq!(indexed_image.get_pixel_aspect_ratio(), (3, 2));

    let resolved_image = resolve_sixel_payload(b"2;1q\"6;4;1;1@", [0, 0, 0]);
    assert_eq!(
        (resolved_image.pixel_width, resolved_image.pixel_height),
        (2, 3)
    );
}

#[test]
fn sixel_raster_declarations_keep_data_and_first_pre_data_declaration() {
    let sixel_graphic = parse_sixel_payload(b"1;1q\"1;1;1;1\"1;1;2;2@@");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("set bits have an extent");
    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (2, 2)
    );
    assert_eq!(indexed_image.get_pixel_aspect_ratio(), (1, 1));

    let terminal_background_graphic = parse_sixel_payload(b"1;0q\"1;1;1;1@");
    let preserve_background_graphic = parse_sixel_payload(b"1;1q\"1;1;1;1@");
    assert_eq!(
        (
            terminal_background_graphic
                .get_indexed_image()
                .expect("data")
                .get_width_pixels(),
            terminal_background_graphic
                .get_indexed_image()
                .expect("data")
                .get_height_pixels()
        ),
        (1, 6)
    );
    assert_eq!(
        (
            preserve_background_graphic
                .get_indexed_image()
                .expect("data")
                .get_width_pixels(),
            preserve_background_graphic
                .get_indexed_image()
                .expect("data")
                .get_height_pixels()
        ),
        (1, 1)
    );

    let beyond_declared_graphic = parse_sixel_payload(b"7;1q\"1;1;1;1~~");
    assert_eq!(
        (
            beyond_declared_graphic
                .get_indexed_image()
                .expect("data beyond declaration")
                .get_width_pixels(),
            beyond_declared_graphic
                .get_indexed_image()
                .expect("data beyond declaration")
                .get_height_pixels()
        ),
        (2, 6)
    );
}

#[test]
fn sixel_terminal_data_beyond_declared_rectangle_stays_in_the_image() {
    let sixel_graphic = parse_sixel_payload(b"1;0q\"1;1;1;1~~");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("two terminal data columns have an extent");

    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (2, 6)
    );
    assert_eq!(indexed_image.pixel_register_indices, [0; 12]);
}

#[test]
fn sixel_canvas_resize_preserves_a_wide_first_band_before_a_narrow_band() {
    let sixel_graphic = parse_sixel_payload(b"1;1q#1;2;100;0;0!9000@#2;2;0;100;0-@");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("both bands have set pixels");

    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (9000, 7)
    );
    assert_eq!(indexed_image.pixel_register_indices[8999], 1);
    assert_eq!(indexed_image.pixel_register_indices[6 * 9000], 2);
}

#[test]
fn sixel_canvas_resize_preserves_a_tall_first_band_before_a_wide_band() {
    let mut sixel_payload_bytes = b"7;1q#1;2;100;0;0".to_vec();
    sixel_payload_bytes.extend(std::iter::repeat_n(b'-', 1365));
    sixel_payload_bytes.extend_from_slice(b"~$#2;2;0;100;0!2000@");
    let sixel_graphic = parse_sixel_payload(&sixel_payload_bytes);
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("both bands have set pixels");

    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (2000, 8196)
    );
    assert_eq!(indexed_image.pixel_register_indices[8195 * 2000], 1);
    assert_eq!(indexed_image.pixel_register_indices[8190 * 2000 + 1999], 2);
}

#[test]
fn sixel_preserve_mode_checks_the_highest_set_bit_at_the_side_limit() {
    let mut within_limit_payload_bytes = b"7;1q".to_vec();
    within_limit_payload_bytes.extend(std::iter::repeat_n(b'-', 2730));
    within_limit_payload_bytes.push(b'@');
    let sixel_graphic = parse_sixel_payload(&within_limit_payload_bytes);
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("the top bit is inside the limit");
    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (1, 16_381)
    );
    assert_eq!(indexed_image.pixel_register_indices[16_380], 0);

    let mut beyond_limit_payload_bytes = b"7;1q".to_vec();
    beyond_limit_payload_bytes.extend(std::iter::repeat_n(b'-', 2730));
    beyond_limit_payload_bytes.push(b'~');
    assert_sixel_image_too_large_error(
        parse_sixel_result(&beyond_limit_payload_bytes)
            .expect_err("the sixel reaches beyond the side limit"),
    );

    let mut zero_bit_payload_bytes = b"7;1q".to_vec();
    zero_bit_payload_bytes.extend(std::iter::repeat_n(b'-', 2730));
    zero_bit_payload_bytes.push(b'?');
    assert!(
        parse_sixel_payload(&zero_bit_payload_bytes)
            .get_indexed_image()
            .is_none(),
        "zero bits do not create a preserve-mode extent"
    );
}

#[test]
fn sixel_declared_terminal_background_fills_only_its_rectangle() {
    let sixel_graphic = parse_sixel_payload(b"1;0q\"1;1;3;2@");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("declared dimensions provide an extent");

    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (3, 6)
    );
    assert_eq!(
        &indexed_image.pixel_register_indices[0..3],
        [0, SIXEL_TERMINAL_BACKGROUND, SIXEL_TERMINAL_BACKGROUND]
    );
    assert_eq!(
        &indexed_image.pixel_register_indices[3..6],
        [SIXEL_TERMINAL_BACKGROUND; 3]
    );
    assert_eq!(
        &indexed_image.pixel_register_indices[6..9],
        [SIXEL_TERMINAL_BACKGROUND, SIXEL_UNTOUCHED, SIXEL_UNTOUCHED]
    );
}

#[test]
fn sixel_terminal_background_keeps_unvisited_narrow_band_gaps_untouched() {
    let sixel_graphic = parse_sixel_payload(b"1;0q????-?");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("both bands have an extent");

    assert_eq!(
        (
            indexed_image.get_width_pixels(),
            indexed_image.get_height_pixels()
        ),
        (4, 12)
    );
    assert_eq!(
        &indexed_image.pixel_register_indices[0..4],
        [SIXEL_TERMINAL_BACKGROUND; 4]
    );
    assert_eq!(
        &indexed_image.pixel_register_indices[24..28],
        [
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_UNTOUCHED,
            SIXEL_UNTOUCHED,
            SIXEL_UNTOUCHED
        ]
    );
    for indexed_pixel_row_index in 7..12 {
        assert_eq!(
            &indexed_image.pixel_register_indices
                [indexed_pixel_row_index * 4..indexed_pixel_row_index * 4 + 4],
            [
                SIXEL_TERMINAL_BACKGROUND,
                SIXEL_UNTOUCHED,
                SIXEL_UNTOUCHED,
                SIXEL_UNTOUCHED
            ]
        );
    }
}

#[test]
fn sixel_redefined_register_resolves_prior_pixels_with_final_color() {
    let sixel_graphic = parse_sixel_payload(b"7;1q#1;2;100;0;0#1@#1;2;0;0;100");
    assert_eq!(
        sixel_graphic
            .get_palette_changes()
            .list_palette_changes()
            .len(),
        1
    );
    let sixel_palette_change = sixel_graphic.get_palette_changes().list_palette_changes()[0];
    assert_eq!(sixel_palette_change.register_number, 1);
    assert_eq!(sixel_palette_change.rgb_color, [0, 0, 255]);

    let resolved_image = resolve_sixel_payload(b"7;1q#1;2;100;0;0#1@#1;2;0;0;100", [0, 0, 0]);
    assert_eq!(resolved_image.rgba_bytes, [0, 0, 255, 255]);
}

#[test]
fn sixel_terminal_zero_bits_do_not_repaint_an_existing_foreground() {
    let sixel_graphic = parse_sixel_payload(b"7;0q#1;2;100;0;0#1@$?");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("data has an extent");

    assert_eq!(indexed_image.pixel_register_indices[0], 1);
    let mut sixel_palette = SixelPalette::default();
    sixel_palette.apply_palette_changes(sixel_graphic.get_palette_changes());
    let resolved_image = indexed_image
        .resolve_indexed_image(&sixel_palette, [9, 8, 7])
        .expect("image resolves");
    assert_eq!(&resolved_image.rgba_bytes[..4], [255, 0, 0, 255]);
}

#[test]
fn sixel_hls_color_definitions_convert_to_rgb() {
    let sixel_graphic = parse_sixel_payload(b"q#1;1;0;50;100#1@");

    assert_eq!(
        sixel_graphic.get_palette_changes().list_palette_changes()[0].rgb_color,
        [0, 0, 255]
    );
}

#[test]
fn sixel_palette_only_and_blank_payloads_return_metadata_without_an_image() {
    let palette_only_graphic = parse_sixel_payload(b"q#3;2;100;50;0");
    assert!(palette_only_graphic.get_indexed_image().is_none());
    assert_eq!(
        palette_only_graphic.get_sixel_background(),
        SixelBackground::Terminal
    );
    assert_eq!(
        palette_only_graphic
            .get_palette_changes()
            .list_palette_changes()
            .len(),
        1
    );

    let blank_graphic = parse_sixel_payload(b"1;1q");
    assert!(blank_graphic.get_indexed_image().is_none());
    assert!(blank_graphic
        .get_palette_changes()
        .list_palette_changes()
        .is_empty());
}

#[test]
fn sixel_split_streams_preserve_header_commands_and_pixel_data() {
    let sixel_payload_bytes = b"1;1q\"6;4;1;1#1;2;100;0;0#1@";
    let mut sixel_parser = SixelParser::new();
    for sixel_payload_chunk in sixel_payload_bytes.chunks(3) {
        for &sixel_payload_byte in sixel_payload_chunk {
            sixel_parser
                .feed_input_byte(sixel_payload_byte)
                .expect("split Sixel payload is valid");
        }
    }
    let sixel_graphic = sixel_parser
        .finish_payload()
        .expect("split payload finishes");
    assert_eq!(
        sixel_graphic
            .get_indexed_image()
            .expect("set bit has an extent")
            .get_pixel_aspect_ratio(),
        (3, 2)
    );
    assert_eq!(
        sixel_graphic.get_palette_changes().list_palette_changes()[0].rgb_color,
        [255, 0, 0]
    );
}

#[test]
fn sixel_whitespace_and_repeat_defaults_follow_xterm_behavior() {
    let spaced_payload_graphic = parse_sixel_payload(b"1;0q!1 0~");
    assert_eq!(
        spaced_payload_graphic
            .get_indexed_image()
            .expect("repeated data has an extent")
            .get_width_pixels(),
        10
    );

    let omitted_repeat_graphic = parse_sixel_payload(b"1;1q!@");
    let zero_repeat_graphic = parse_sixel_payload(b"1;1q!0@");
    assert_eq!(
        omitted_repeat_graphic
            .get_indexed_image()
            .expect("data")
            .get_width_pixels(),
        1
    );
    assert_eq!(
        zero_repeat_graphic
            .get_indexed_image()
            .expect("data")
            .get_width_pixels(),
        1
    );
}

#[test]
fn sixel_zero_raster_aspects_default_independently() {
    let sixel_graphic = parse_sixel_payload(b"7;1q\"0;2;0;0@");
    let indexed_image = sixel_graphic
        .get_indexed_image()
        .expect("set bit has an extent");
    assert_eq!(indexed_image.get_pixel_aspect_ratio(), (1, 2));
}

#[test]
fn sixel_parser_rejects_malformed_commands_and_limits_with_typed_errors() {
    assert_sixel_invalid_command_error(
        parse_sixel_result(b"10q").expect_err("macro parameter is invalid"),
    );
    assert_sixel_invalid_command_error(
        parse_sixel_result(b"1;1q!").expect_err("repeat has no data"),
    );
    assert_sixel_invalid_command_error(
        parse_sixel_result(b"1;1q!2#").expect_err("repeat has no data"),
    );
    assert_sixel_invalid_command_error(
        parse_sixel_result(b"1;1q@\"1;1;1;1@").expect_err("late raster is invalid"),
    );
    assert_sixel_invalid_command_error(
        parse_sixel_result(b"1;1q\x01@").expect_err("unhandled control is invalid"),
    );
    assert_sixel_invalid_command_error(
        parse_sixel_result(b"1;1q#1;2;3;4;5;6@").expect_err("too many color parameters"),
    );
    assert_sixel_image_too_large_error(
        parse_sixel_result(b"1;1q!20000@").expect_err("repeat exceeds the image side bound"),
    );
    assert_sixel_image_too_large_error(
        parse_sixel_result(b"1;1q\"1;1;16385;0@").expect_err("declared width exceeds the bound"),
    );
}

#[test]
fn sixel_numeric_overflow_and_control_size_fail_without_panics() {
    assert_sixel_invalid_command_error(
        parse_sixel_result(b"999999999999999999999999q").expect_err("header number overflows"),
    );
    let mut sixel_payload_bytes = b"1;1q!".to_vec();
    sixel_payload_bytes.extend(std::iter::repeat_n(
        b'1',
        MAX_GRAPHICS_CONTROL_BYTE_COUNT + 1,
    ));
    sixel_payload_bytes.push(b'@');
    match parse_sixel_result(&sixel_payload_bytes).expect_err("repeat control exceeds its bound") {
        GraphicsError::TransferTooLarge { protocol } => {
            assert_eq!(protocol, GraphicsProtocol::Sixel);
        }
        unexpected_sixel_graphics_error => {
            panic!("unexpected error: {unexpected_sixel_graphics_error:?}")
        }
    }

    let mut sixel_parser = SixelParser::new();
    sixel_parser.received_byte_count = MAX_GRAPHICS_TRANSFER_BYTE_COUNT;
    match sixel_parser
        .feed_input_byte(b'q')
        .expect_err("input transfer exceeds its bound")
    {
        GraphicsError::TransferTooLarge { protocol } => {
            assert_eq!(protocol, GraphicsProtocol::Sixel);
        }
        unexpected_sixel_graphics_error => {
            panic!("unexpected error: {unexpected_sixel_graphics_error:?}")
        }
    }
}

#[test]
fn sixel_serde_round_trip_preserves_indexed_graphic_and_palette() {
    let original_graphic = parse_sixel_payload(b"1;0q\"6;4;3;2#1;2;100;0;0#1@");
    let serialized_graphic = serde_json::to_string(&original_graphic).expect("graphic serializes");
    let deserialized_graphic: SixelGraphic =
        serde_json::from_str(&serialized_graphic).expect("graphic deserializes");
    assert_eq!(deserialized_graphic, original_graphic);

    let default_sixel_palette = SixelPalette::default();
    let serialized_sixel_palette =
        serde_json::to_string(&default_sixel_palette).expect("palette serializes");
    let deserialized_palette: SixelPalette =
        serde_json::from_str(&serialized_sixel_palette).expect("palette deserializes");
    assert_eq!(deserialized_palette, default_sixel_palette);
}

#[test]
fn sixel_index_deserialization_grows_only_when_capacity_is_full() {
    let serialized_pixel_register_indices =
        serde_json::to_string(&vec![0u16; 1024]).expect("indices serialize");
    let bounded_pixel_register_indices: BoundedPixelRegisterIndices =
        serde_json::from_str(&serialized_pixel_register_indices).expect("indices deserialize");

    assert_eq!(bounded_pixel_register_indices.0.len(), 1024);
    assert!(bounded_pixel_register_indices.0.capacity() <= 2048);
}

#[test]
fn sixel_serde_rejects_duplicate_palette_changes_invalid_indices_and_wrong_lengths() {
    let duplicate_palette_change_error = serde_json::from_str::<SixelPaletteChanges>(
        "[{\"register_number\":1,\"rgb_color\":[1,2,3]},{\"register_number\":1,\"rgb_color\":[4,5,6]}]",
    )
    .expect_err("duplicate palette edits are invalid");
    assert!(duplicate_palette_change_error
        .to_string()
        .contains("duplicate register"));

    let serialized_too_many_palette_colors =
        serde_json::to_string(&vec![[0u8, 0, 0]; 257]).expect("colors serialize");
    let too_many_palette_color_error =
        serde_json::from_str::<SixelPalette>(&serialized_too_many_palette_colors)
            .expect_err("palette must have exactly 256 colors");
    assert!(too_many_palette_color_error
        .to_string()
        .contains("more than 256"));

    let mut serialized_too_many_palette_changes = String::from("[");
    for register_number in 0..=u8::MAX {
        if register_number != 0 {
            serialized_too_many_palette_changes.push(',');
        }
        serialized_too_many_palette_changes.push_str(&format!(
            "{{\"register_number\":{register_number},\"rgb_color\":[0,0,0]}}"
        ));
    }
    serialized_too_many_palette_changes.push_str(",{\"register_number\":0,\"rgb_color\":[0,0,0]}]");
    let too_many_palette_change_error =
        serde_json::from_str::<SixelPaletteChanges>(&serialized_too_many_palette_changes)
            .expect_err("palette changes must stay within the register bound");
    assert!(too_many_palette_change_error
        .to_string()
        .contains("exceed 256 registers"));

    let invalid_register_index_error = serde_json::from_str::<IndexedImage>(
        "{\"width_pixels\":1,\"height_pixels\":1,\"pixel_register_indices\":[258],\"pixel_aspect_vertical\":1,\"pixel_aspect_horizontal\":1}",
    )
    .expect_err("index above the sentinel range is invalid");
    assert!(invalid_register_index_error
        .to_string()
        .contains("invalid register or sentinel"));

    let wrong_index_length_error = serde_json::from_str::<IndexedImage>(
        "{\"width_pixels\":2,\"height_pixels\":1,\"pixel_register_indices\":[0],\"pixel_aspect_vertical\":1,\"pixel_aspect_horizontal\":1}",
    )
    .expect_err("index length must match dimensions");
    assert!(wrong_index_length_error
        .to_string()
        .contains("dimensions are invalid"));

    let serialized_too_few_palette_colors =
        serde_json::to_string(&vec![[0u8, 0, 0]; 255]).expect("colors serialize");
    let too_few_palette_color_error =
        serde_json::from_str::<SixelPalette>(&serialized_too_few_palette_colors)
            .expect_err("palette must have exactly 256 colors");
    assert!(too_few_palette_color_error
        .to_string()
        .contains("fewer than 256"));

    let unnormalized_aspect_error = serde_json::from_str::<IndexedImage>(
        "{\"width_pixels\":1,\"height_pixels\":1,\"pixel_register_indices\":[0],\"pixel_aspect_vertical\":2,\"pixel_aspect_horizontal\":2}",
    )
    .expect_err("aspect ratio must be normalized");
    assert!(unnormalized_aspect_error
        .to_string()
        .contains("dimensions are invalid"));

    let oversized_aspect_error = serde_json::from_str::<IndexedImage>(
        "{\"width_pixels\":1,\"height_pixels\":1,\"pixel_register_indices\":[0],\"pixel_aspect_vertical\":16385,\"pixel_aspect_horizontal\":1}",
    )
    .expect_err("expanded aspect must stay within image limits");
    assert!(oversized_aspect_error
        .to_string()
        .contains("image is too large"));
}
