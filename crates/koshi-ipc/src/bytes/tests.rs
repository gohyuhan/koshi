//! Tests for bytes carried as one base64 string: the text every byte length
//! produces, the bytes every text reads back as, and the exact reason each
//! malformed text is refused. Also the lowercase hex a byte slice writes as.

use super::*;

/// Decode `text` through the same path a wire field takes.
fn deserialize_base64_text(encoded_text: &str) -> Result<Vec<u8>, String> {
    let serialized_json = serde_json::to_string(encoded_text).expect("a string encodes");
    let mut json_reader = serde_json::Deserializer::from_str(&serialized_json);
    deserialize(&mut json_reader).map_err(|deserialize_error| deserialize_error.to_string())
}

#[test]
fn every_group_length_encodes_to_the_text_rfc_4648_pins() {
    assert_eq!(encode_base64_text(&[]), "");
    assert_eq!(encode_base64_text(&[104]), "aA==");
    assert_eq!(encode_base64_text(&[104, 105]), "aGk=");
    assert_eq!(encode_base64_text(&[104, 105, 33]), "aGkh");
    assert_eq!(encode_base64_text(&[104, 105, 33, 10]), "aGkhCg==");
    // A whole group before the last one, so a last group shorter than three
    // bytes is written past the groups already there, and only the characters
    // it does not reach stay padding.
    assert_eq!(encode_base64_text(&[104, 105, 33, 10, 13]), "aGkhCg0=");
    assert_eq!(encode_base64_text(&[104, 105, 33, 10, 13, 32]), "aGkhCg0g");
    assert_eq!(
        encode_base64_text(&[104, 105, 33, 10, 13, 32, 65]),
        "aGkhCg0gQQ=="
    );
    assert_eq!(encode_base64_text(&[0, 0, 0]), "AAAA");
    assert_eq!(encode_base64_text(&[255, 255, 255]), "////");
    assert_eq!(encode_base64_text(&[251, 255, 190]), "+/++");
}

#[test]
fn every_byte_value_survives_a_round_trip() {
    let byte_values: Vec<u8> = (0..=255).collect();
    assert_eq!(
        decode_base64_text(&encode_base64_text(&byte_values)),
        Ok(byte_values.clone())
    );
    // Every remainder of the three-byte group, so each padding case is read
    // back as the bytes it was written from.
    assert_eq!(
        decode_base64_text(&encode_base64_text(&byte_values[..254])),
        Ok(byte_values[..254].to_vec())
    );
    assert_eq!(
        decode_base64_text(&encode_base64_text(&byte_values[..255])),
        Ok(byte_values[..255].to_vec())
    );
}

#[test]
fn a_terminal_chunk_round_trips_through_the_wire_path() {
    let chunk_bytes: Vec<u8> = (0..8192u32)
        .map(|byte_index| (byte_index % 256) as u8)
        .collect();
    let encoded_chunk_text = encode_base64_text(&chunk_bytes);
    assert_eq!(encoded_chunk_text.len(), 10924);
    assert_eq!(
        deserialize_base64_text(&encoded_chunk_text),
        Ok(chunk_bytes)
    );
}

#[test]
fn text_that_is_not_base64_is_refused_by_reason() {
    assert_eq!(
        decode_base64_text("aGk"),
        Err("the base64 text is not padded to a multiple of four characters")
    );
    assert_eq!(
        decode_base64_text("a==="),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
    assert_eq!(
        decode_base64_text("aG-k"),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
    assert_eq!(
        decode_base64_text("a=Gk"),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
    assert_eq!(
        decode_base64_text("aB=="),
        Err("the base64 text ends with unused bits that are not zero")
    );
    assert_eq!(
        decode_base64_text("aGl="),
        Err("the base64 text ends with unused bits that are not zero")
    );
}

#[test]
fn no_text_at_all_reads_back_as_no_bytes() {
    // A pane that printed nothing carries an empty chunk, which encodes to an
    // empty string and must read back as the empty chunk it was.
    assert_eq!(decode_base64_text(""), Ok(Vec::new()));
    assert_eq!(deserialize_base64_text(""), Ok(Vec::new()));
}

#[test]
fn text_that_is_padding_the_whole_way_across_is_refused() {
    assert_eq!(
        decode_base64_text("===="),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
}

#[test]
fn a_newline_after_the_text_is_refused_rather_than_trimmed() {
    // A chunk of a child's output is carried exactly, so text with anything
    // around it is not the text this build wrote.
    assert_eq!(
        decode_base64_text("aGk=\n"),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
    assert_eq!(
        decode_base64_text("aG k"),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
}

#[test]
fn a_reason_reaches_the_caller_as_a_decoding_error() {
    assert_eq!(
        deserialize_base64_text("aG-k"),
        Err(
            "the base64 text holds a character the alphabet does not allow there at line 1 column 6"
                .to_string()
        )
    );
}

#[test]
fn a_value_that_is_not_a_string_is_refused() {
    let mut json_reader = serde_json::Deserializer::from_str("[104,105]");
    let deserialize_error =
        deserialize(&mut json_reader).expect_err("an array is not a base64 string");
    assert_eq!(
        deserialize_error.to_string(),
        "invalid type: sequence, expected bytes as a base64 string at line 1 column 0"
    );
}

#[test]
fn text_whose_last_group_is_one_character_is_refused() {
    assert_eq!(
        decode_base64_text("a"),
        Err("the base64 text length is not a multiple of four")
    );
    assert_eq!(
        decode_base64_text("aGkhC"),
        Err("the base64 text length is not a multiple of four")
    );
}

#[test]
fn serializing_writes_one_base64_string() {
    let mut serialized_json_bytes = Vec::new();
    let mut json_writer = serde_json::Serializer::new(&mut serialized_json_bytes);

    serialize(&[104, 105], &mut json_writer).expect("a string serializes");

    assert_eq!(serialized_json_bytes, br#""aGk=""#);
}

#[test]
fn serializing_no_bytes_writes_an_empty_string() {
    let mut serialized_json_bytes = Vec::new();
    let mut json_writer = serde_json::Serializer::new(&mut serialized_json_bytes);

    serialize(&[], &mut json_writer).expect("a string serializes");

    assert_eq!(serialized_json_bytes, br#""""#);
}

#[test]
fn format_hex_writes_two_lowercase_characters_per_byte() {
    assert_eq!(format_hex(&[]), "");
    assert_eq!(format_hex(&[0]), "00");
    assert_eq!(format_hex(&[104, 105]), "6869");
    assert_eq!(format_hex(&[0, 15, 16, 171, 255]), "000f10abff");
}

#[test]
fn format_hex_of_every_byte_value_writes_bytes_in_order() {
    let byte_values: Vec<u8> = (0..=255).collect();
    let encoded_hex_text = format_hex(&byte_values);
    assert_eq!(encoded_hex_text.len(), 512);
    assert_eq!(&encoded_hex_text[..8], "00010203");
    assert_eq!(&encoded_hex_text[504..], "fcfdfeff");
    assert_eq!(encoded_hex_text.to_ascii_lowercase(), encoded_hex_text);
}
