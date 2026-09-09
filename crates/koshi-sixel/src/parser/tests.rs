//! Contract tests for incremental indexed Sixel parsing.

use super::*;

fn parse(payload: &[u8]) -> SixelGraphic {
    parse_result(payload).expect("Sixel payload is valid")
}

fn parse_result(payload: &[u8]) -> Result<SixelGraphic, GraphicsError> {
    let mut parser = SixelParser::new();
    for &byte in payload {
        parser.feed(byte)?;
    }
    parser.finish()
}

fn resolve(payload: &[u8], background: [u8; 3]) -> DecodedImage {
    let graphic = parse(payload);
    let image = graphic.image().expect("payload has an indexed image");
    let mut palette = SixelPalette::default();
    palette.apply_changes(graphic.palette_changes());
    image.resolve(&palette, background).expect("image resolves")
}

fn assert_invalid_command(error: GraphicsError) {
    match error {
        GraphicsError::InvalidCommand { protocol } => {
            assert_eq!(protocol, GraphicsProtocol::Sixel);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

fn assert_image_too_large(error: GraphicsError) {
    match error {
        GraphicsError::ImageTooLarge { protocol } => {
            assert_eq!(protocol, GraphicsProtocol::Sixel);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn preserves_register_indices_and_terminal_background_rows() {
    let graphic = parse(b"q#1;2;100;0;0#1@");
    let image = graphic.image().expect("data column has an extent");

    assert_eq!((image.width(), image.height()), (1, 6));
    assert_eq!(image.pixel_aspect(), (2, 1));
    assert_eq!(
        image.indices,
        [
            1,
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_TERMINAL_BACKGROUND
        ]
    );
    assert_eq!(graphic.background(), SixelBackground::Terminal);

    let resolved = resolve(b"q#1;2;100;0;0#1@", [9, 8, 7]);
    assert_eq!(resolved.width, 1);
    assert_eq!(resolved.height, 12);
    assert_eq!(&resolved.rgba[..4], [255, 0, 0, 255]);
    assert_eq!(&resolved.rgba[4..8], [255, 0, 0, 255]);
    assert_eq!(&resolved.rgba[8..12], [9, 8, 7, 255]);
}

#[test]
fn preserve_mode_uses_set_bit_extent_and_keeps_gaps_transparent() {
    let graphic = parse(b"1;1q@-@");
    let image = graphic.image().expect("set bits have an extent");

    assert_eq!((image.width(), image.height()), (1, 7));
    assert_eq!(image.indices[0], 0);
    assert_eq!(&image.indices[1..6], [SIXEL_UNTOUCHED; 5]);
    assert_eq!(image.indices[6], 0);

    let resolved = resolve(b"7;1q@-@", [9, 8, 7]);
    assert_eq!(resolved.height, 7);
    assert_eq!(&resolved.rgba[4..24], [0, 0, 0, 0].repeat(5).as_slice());
}

#[test]
fn preserve_zero_bits_with_declared_extent_do_not_paint_or_overrun() {
    let graphic = parse(b"7;1q\"1;1;2;2?");
    let image = graphic
        .image()
        .expect("declared dimensions provide an extent");

    assert_eq!((image.width(), image.height()), (2, 2));
    assert_eq!(image.indices, [SIXEL_UNTOUCHED; 4]);
}

#[test]
fn macro_aspects_match_the_dec_table() {
    let expected = [
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
    for (parameter, aspect) in expected {
        let payload = format!("{parameter};1q@");
        let graphic = parse(payload.as_bytes());
        assert_eq!(
            graphic
                .image()
                .expect("set bit has an extent")
                .pixel_aspect(),
            aspect
        );
    }
    let omitted = parse(b"q@");
    assert_eq!(
        omitted
            .image()
            .expect("set bit has an extent")
            .pixel_aspect(),
        (2, 1)
    );
}

#[test]
fn raster_aspect_is_reduced_and_overrides_macro_aspect() {
    let graphic = parse(b"2;1q\"6;4;1;1@");
    let image = graphic.image().expect("set bit has an extent");
    assert_eq!(image.pixel_aspect(), (3, 2));

    let resolved = resolve(b"2;1q\"6;4;1;1@", [0, 0, 0]);
    assert_eq!((resolved.width, resolved.height), (2, 3));
}

#[test]
fn raster_declarations_never_clip_data_and_repeat_pre_data_wins() {
    let graphic = parse(b"1;1q\"1;1;1;1\"1;1;2;2@@");
    let image = graphic.image().expect("set bits have an extent");
    assert_eq!((image.width(), image.height()), (2, 2));
    assert_eq!(image.pixel_aspect(), (1, 1));

    let terminal = parse(b"1;0q\"1;1;1;1@");
    let preserve = parse(b"1;1q\"1;1;1;1@");
    assert_eq!(
        (
            terminal.image().expect("data").width(),
            terminal.image().expect("data").height()
        ),
        (1, 6)
    );
    assert_eq!(
        (
            preserve.image().expect("data").width(),
            preserve.image().expect("data").height()
        ),
        (1, 1)
    );

    let beyond = parse(b"7;1q\"1;1;1;1~~");
    assert_eq!(
        (
            beyond.image().expect("data beyond declaration").width(),
            beyond.image().expect("data beyond declaration").height()
        ),
        (2, 6)
    );
}

#[test]
fn terminal_data_beyond_declared_rectangle_stays_in_the_image() {
    let graphic = parse(b"1;0q\"1;1;1;1~~");
    let image = graphic
        .image()
        .expect("two terminal data columns have an extent");

    assert_eq!((image.width(), image.height()), (2, 6));
    assert_eq!(image.indices, [0; 12]);
}

#[test]
fn resize_preserves_a_wide_first_band_before_a_narrow_band() {
    let graphic = parse(b"1;1q#1;2;100;0;0!9000@#2;2;0;100;0-@");
    let image = graphic.image().expect("both bands have set pixels");

    assert_eq!((image.width(), image.height()), (9000, 7));
    assert_eq!(image.indices[8999], 1);
    assert_eq!(image.indices[6 * 9000], 2);
}

#[test]
fn resize_preserves_a_tall_first_band_before_a_wide_band() {
    let mut payload = b"7;1q#1;2;100;0;0".to_vec();
    payload.extend(std::iter::repeat_n(b'-', 1365));
    payload.extend_from_slice(b"~$#2;2;0;100;0!2000@");
    let graphic = parse(&payload);
    let image = graphic.image().expect("both bands have set pixels");

    assert_eq!((image.width(), image.height()), (2000, 8196));
    assert_eq!(image.indices[8195 * 2000], 1);
    assert_eq!(image.indices[8190 * 2000 + 1999], 2);
}

#[test]
fn preserve_mode_checks_the_highest_set_bit_at_the_side_limit() {
    let mut valid = b"7;1q".to_vec();
    valid.extend(std::iter::repeat_n(b'-', 2730));
    valid.push(b'@');
    let graphic = parse(&valid);
    let image = graphic.image().expect("the top bit is inside the limit");
    assert_eq!((image.width(), image.height()), (1, 16_381));
    assert_eq!(image.indices[16_380], 0);

    let mut invalid = b"7;1q".to_vec();
    invalid.extend(std::iter::repeat_n(b'-', 2730));
    invalid.push(b'~');
    assert_image_too_large(
        parse_result(&invalid).expect_err("the sixel reaches beyond the side limit"),
    );

    let mut transparent = b"7;1q".to_vec();
    transparent.extend(std::iter::repeat_n(b'-', 2730));
    transparent.push(b'?');
    assert!(
        parse(&transparent).image().is_none(),
        "zero bits do not create a preserve-mode extent"
    );
}

#[test]
fn declared_background_fills_only_its_rectangle() {
    let graphic = parse(b"1;0q\"1;1;3;2@");
    let image = graphic
        .image()
        .expect("declared dimensions provide an extent");

    assert_eq!((image.width(), image.height()), (3, 6));
    assert_eq!(
        &image.indices[0..3],
        [0, SIXEL_TERMINAL_BACKGROUND, SIXEL_TERMINAL_BACKGROUND]
    );
    assert_eq!(&image.indices[3..6], [SIXEL_TERMINAL_BACKGROUND; 3]);
    assert_eq!(
        &image.indices[6..9],
        [SIXEL_TERMINAL_BACKGROUND, SIXEL_UNTOUCHED, SIXEL_UNTOUCHED]
    );
}

#[test]
fn unvisited_gaps_after_a_narrower_band_remain_untouched() {
    let graphic = parse(b"1;0q????-?");
    let image = graphic.image().expect("both bands have an extent");

    assert_eq!((image.width(), image.height()), (4, 12));
    assert_eq!(&image.indices[0..4], [SIXEL_TERMINAL_BACKGROUND; 4]);
    assert_eq!(
        &image.indices[24..28],
        [
            SIXEL_TERMINAL_BACKGROUND,
            SIXEL_UNTOUCHED,
            SIXEL_UNTOUCHED,
            SIXEL_UNTOUCHED
        ]
    );
    for row in 7..12 {
        assert_eq!(
            &image.indices[row * 4..row * 4 + 4],
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
fn redefined_register_resolves_prior_pixels_with_final_color() {
    let graphic = parse(b"7;1q#1;2;100;0;0#1@#1;2;0;0;100");
    assert_eq!(graphic.palette_changes().entries().len(), 1);
    let change = graphic.palette_changes().entries()[0];
    assert_eq!(change.register(), 1);
    assert_eq!(change.color(), [0, 0, 255]);

    let resolved = resolve(b"7;1q#1;2;100;0;0#1@#1;2;0;0;100", [0, 0, 0]);
    assert_eq!(resolved.rgba, [0, 0, 255, 255]);
}

#[test]
fn terminal_zero_bits_do_not_repaint_an_existing_foreground() {
    let graphic = parse(b"7;0q#1;2;100;0;0#1@$?");
    let image = graphic.image().expect("data has an extent");

    assert_eq!(image.indices[0], 1);
    let mut palette = SixelPalette::default();
    palette.apply_changes(graphic.palette_changes());
    let resolved = image.resolve(&palette, [9, 8, 7]).expect("image resolves");
    assert_eq!(&resolved.rgba[..4], [255, 0, 0, 255]);
}

#[test]
fn hls_color_definitions_convert_to_rgb() {
    let graphic = parse(b"q#1;1;0;50;100#1@");

    assert_eq!(graphic.palette_changes().entries()[0].color(), [0, 0, 255]);
}

#[test]
fn palette_only_and_blank_payloads_return_metadata_without_an_image() {
    let palette_only = parse(b"q#3;2;100;50;0");
    assert!(palette_only.image().is_none());
    assert_eq!(palette_only.background(), SixelBackground::Terminal);
    assert_eq!(palette_only.palette_changes().entries().len(), 1);

    let blank = parse(b"1;1q");
    assert!(blank.image().is_none());
    assert!(blank.palette_changes().entries().is_empty());
}

#[test]
fn split_streams_preserve_header_commands_and_data() {
    let payload = b"1;1q\"6;4;1;1#1;2;100;0;0#1@";
    let mut parser = SixelParser::new();
    for chunk in payload.chunks(3) {
        for &byte in chunk {
            parser.feed(byte).expect("split Sixel payload is valid");
        }
    }
    let graphic = parser.finish().expect("split payload finishes");
    assert_eq!(
        graphic
            .image()
            .expect("set bit has an extent")
            .pixel_aspect(),
        (3, 2)
    );
    assert_eq!(graphic.palette_changes().entries()[0].color(), [255, 0, 0]);
}

#[test]
fn whitespace_and_repeat_defaults_follow_xterm_behavior() {
    let spaced = parse(b"1;0q!1 0~");
    assert_eq!(
        spaced.image().expect("repeated data has an extent").width(),
        10
    );

    let omitted = parse(b"1;1q!@");
    let zero = parse(b"1;1q!0@");
    assert_eq!(omitted.image().expect("data").width(), 1);
    assert_eq!(zero.image().expect("data").width(), 1);
}

#[test]
fn zero_raster_aspects_default_independently() {
    let graphic = parse(b"7;1q\"0;2;0;0@");
    let image = graphic.image().expect("set bit has an extent");
    assert_eq!(image.pixel_aspect(), (1, 2));
}

#[test]
fn malformed_commands_and_limits_return_typed_errors() {
    assert_invalid_command(parse_result(b"10q").expect_err("macro parameter is invalid"));
    assert_invalid_command(parse_result(b"1;1q!").expect_err("repeat has no data"));
    assert_invalid_command(parse_result(b"1;1q!2#").expect_err("repeat has no data"));
    assert_invalid_command(parse_result(b"1;1q@\"1;1;1;1@").expect_err("late raster is invalid"));
    assert_invalid_command(parse_result(b"1;1q\x01@").expect_err("unhandled control is invalid"));
    assert_invalid_command(
        parse_result(b"1;1q#1;2;3;4;5;6@").expect_err("too many color parameters"),
    );
    assert_image_too_large(
        parse_result(b"1;1q!20000@").expect_err("repeat exceeds the image side bound"),
    );
    assert_image_too_large(
        parse_result(b"1;1q\"1;1;16385;0@").expect_err("declared width exceeds the bound"),
    );
}

#[test]
fn numeric_overflow_and_control_size_fail_without_panics() {
    assert_invalid_command(
        parse_result(b"999999999999999999999999q").expect_err("header number overflows"),
    );
    let mut payload = b"1;1q!".to_vec();
    payload.extend(std::iter::repeat_n(b'1', MAX_GRAPHICS_CONTROL_BYTES + 1));
    payload.push(b'@');
    match parse_result(&payload).expect_err("repeat control exceeds its bound") {
        GraphicsError::TransferTooLarge { protocol } => {
            assert_eq!(protocol, GraphicsProtocol::Sixel);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let mut parser = SixelParser::new();
    parser.input_bytes = MAX_GRAPHICS_TRANSFER_BYTES;
    match parser
        .feed(b'q')
        .expect_err("input transfer exceeds its bound")
    {
        GraphicsError::TransferTooLarge { protocol } => {
            assert_eq!(protocol, GraphicsProtocol::Sixel);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn serde_round_trip_preserves_indexed_graphic() {
    let original = parse(b"1;0q\"6;4;3;2#1;2;100;0;0#1@");
    let encoded = serde_json::to_string(&original).expect("graphic serializes");
    let decoded: SixelGraphic = serde_json::from_str(&encoded).expect("graphic deserializes");
    assert_eq!(decoded, original);

    let palette = SixelPalette::default();
    let encoded_palette = serde_json::to_string(&palette).expect("palette serializes");
    let decoded_palette: SixelPalette =
        serde_json::from_str(&encoded_palette).expect("palette deserializes");
    assert_eq!(decoded_palette, palette);
}

#[test]
fn bounded_index_deserialization_grows_only_when_full() {
    let encoded = serde_json::to_string(&vec![0u16; 1024]).expect("indices serialize");
    let decoded: BoundedIndices = serde_json::from_str(&encoded).expect("indices deserialize");

    assert_eq!(decoded.0.len(), 1024);
    assert!(decoded.0.capacity() <= 2048);
}

#[test]
fn serde_rejects_duplicate_changes_bad_indices_and_wrong_lengths() {
    let duplicate = serde_json::from_str::<SixelPaletteChanges>(
        "[{\"register\":1,\"color\":[1,2,3]},{\"register\":1,\"color\":[4,5,6]}]",
    )
    .expect_err("duplicate palette edits are invalid");
    assert!(duplicate.to_string().contains("duplicate register"));

    let too_many_colors = serde_json::to_string(&vec![[0u8, 0, 0]; 257]).expect("colors serialize");
    let error = serde_json::from_str::<SixelPalette>(&too_many_colors)
        .expect_err("palette must have exactly 256 colors");
    assert!(error.to_string().contains("more than 256"));

    let mut too_many_changes = String::from("[");
    for register in 0..=u8::MAX {
        if register != 0 {
            too_many_changes.push(',');
        }
        too_many_changes.push_str(&format!("{{\"register\":{register},\"color\":[0,0,0]}}"));
    }
    too_many_changes.push_str(",{\"register\":0,\"color\":[0,0,0]}]");
    let error = serde_json::from_str::<SixelPaletteChanges>(&too_many_changes)
        .expect_err("palette changes must stay within the register bound");
    assert!(error.to_string().contains("exceed 256 registers"));

    let bad_index = serde_json::from_str::<IndexedImage>(
        "{\"width\":1,\"height\":1,\"indices\":[258],\"aspect_vertical\":1,\"aspect_horizontal\":1}",
    )
    .expect_err("index above the sentinel range is invalid");
    assert!(bad_index
        .to_string()
        .contains("invalid register or sentinel"));

    let wrong_length = serde_json::from_str::<IndexedImage>(
        "{\"width\":2,\"height\":1,\"indices\":[0],\"aspect_vertical\":1,\"aspect_horizontal\":1}",
    )
    .expect_err("index length must match dimensions");
    assert!(wrong_length.to_string().contains("dimensions are invalid"));

    let too_few_colors = serde_json::to_string(&vec![[0u8, 0, 0]; 255]).expect("colors serialize");
    let error = serde_json::from_str::<SixelPalette>(&too_few_colors)
        .expect_err("palette must have exactly 256 colors");
    assert!(error.to_string().contains("fewer than 256"));

    let unnormalized = serde_json::from_str::<IndexedImage>(
        "{\"width\":1,\"height\":1,\"indices\":[0],\"aspect_vertical\":2,\"aspect_horizontal\":2}",
    )
    .expect_err("aspect ratio must be normalized");
    assert!(unnormalized.to_string().contains("dimensions are invalid"));

    let oversized = serde_json::from_str::<IndexedImage>(
        "{\"width\":1,\"height\":1,\"indices\":[0],\"aspect_vertical\":16385,\"aspect_horizontal\":1}",
    )
    .expect_err("expanded aspect must stay within image limits");
    assert!(oversized.to_string().contains("image is too large"));
}
