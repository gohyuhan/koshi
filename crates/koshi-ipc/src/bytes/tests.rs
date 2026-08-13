//! Tests for bytes carried as one base64 string: the text every byte length
//! produces, the bytes every text reads back as, and the exact reason each
//! malformed text is refused.

use super::*;

/// Decode `text` through the same path a wire field takes.
fn read(text: &str) -> Result<Vec<u8>, String> {
    let json = serde_json::to_string(text).expect("a string encodes");
    let mut reader = serde_json::Deserializer::from_str(&json);
    deserialize(&mut reader).map_err(|error| error.to_string())
}

#[test]
fn every_group_length_encodes_to_the_text_rfc_4648_pins() {
    assert_eq!(encode(&[]), "");
    assert_eq!(encode(&[104]), "aA==");
    assert_eq!(encode(&[104, 105]), "aGk=");
    assert_eq!(encode(&[104, 105, 33]), "aGkh");
    assert_eq!(encode(&[104, 105, 33, 10]), "aGkhCg==");
    // A whole group before the last one, so a last group shorter than three
    // bytes is written past the groups already there, and only the characters
    // it does not reach stay padding.
    assert_eq!(encode(&[104, 105, 33, 10, 13]), "aGkhCg0=");
    assert_eq!(encode(&[104, 105, 33, 10, 13, 32]), "aGkhCg0g");
    assert_eq!(encode(&[104, 105, 33, 10, 13, 32, 65]), "aGkhCg0gQQ==");
    assert_eq!(encode(&[0, 0, 0]), "AAAA");
    assert_eq!(encode(&[255, 255, 255]), "////");
    assert_eq!(encode(&[251, 255, 190]), "+/++");
}

#[test]
fn every_byte_value_survives_a_round_trip() {
    let bytes: Vec<u8> = (0..=255).collect();
    assert_eq!(decode(&encode(&bytes)), Ok(bytes.clone()));
    // Every remainder of the three-byte group, so each padding case is read
    // back as the bytes it was written from.
    assert_eq!(decode(&encode(&bytes[..254])), Ok(bytes[..254].to_vec()));
    assert_eq!(decode(&encode(&bytes[..255])), Ok(bytes[..255].to_vec()));
}

#[test]
fn a_terminal_chunk_round_trips_through_the_wire_path() {
    let chunk: Vec<u8> = (0..8192u32).map(|index| (index % 256) as u8).collect();
    let text = encode(&chunk);
    assert_eq!(text.len(), 10924);
    assert_eq!(read(&text), Ok(chunk));
}

#[test]
fn text_that_is_not_base64_is_refused_by_reason() {
    assert_eq!(
        decode("aGk"),
        Err("the base64 text is not padded to a multiple of four characters")
    );
    assert_eq!(
        decode("a==="),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
    assert_eq!(
        decode("aG-k"),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
    assert_eq!(
        decode("a=Gk"),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
    assert_eq!(
        decode("aB=="),
        Err("the base64 text ends with unused bits that are not zero")
    );
    assert_eq!(
        decode("aGl="),
        Err("the base64 text ends with unused bits that are not zero")
    );
}

#[test]
fn no_text_at_all_reads_back_as_no_bytes() {
    // A pane that printed nothing carries an empty chunk, which encodes to an
    // empty string and must read back as the empty chunk it was.
    assert_eq!(decode(""), Ok(Vec::new()));
    assert_eq!(read(""), Ok(Vec::new()));
}

#[test]
fn text_that_is_padding_the_whole_way_across_is_refused() {
    assert_eq!(
        decode("===="),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
}

#[test]
fn a_newline_after_the_text_is_refused_rather_than_trimmed() {
    // A chunk of a child's output is carried exactly, so text with anything
    // around it is not the text this build wrote.
    assert_eq!(
        decode("aGk=\n"),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
    assert_eq!(
        decode("aG k"),
        Err("the base64 text holds a character the alphabet does not allow there")
    );
}

#[test]
fn a_reason_reaches_the_caller_as_a_decoding_error() {
    assert_eq!(
        read("aG-k"),
        Err(
            "the base64 text holds a character the alphabet does not allow there at line 1 column 6"
                .to_string()
        )
    );
}

#[test]
fn a_value_that_is_not_a_string_is_refused() {
    let mut reader = serde_json::Deserializer::from_str("[104,105]");
    let error = deserialize(&mut reader).expect_err("an array is not a base64 string");
    assert_eq!(
        error.to_string(),
        "invalid type: sequence, expected bytes as a base64 string at line 1 column 0"
    );
}
