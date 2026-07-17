//! OSC 52 encoding tests: exact bytes, base64 roundtrip, and the empty and
//! non-ASCII payloads.

use super::*;

#[test]
fn the_sequence_is_osc_52_c_base64_bel() {
    assert_eq!(osc52_copy("hello"), b"\x1b]52;c;aGVsbG8=\x07");
}

#[test]
fn the_payload_roundtrips_through_base64() {
    let text = "line one\nline two\t– wide 世界";
    let sequence = osc52_copy(text);
    let payload = &sequence[b"\x1b]52;c;".len()..sequence.len() - 1];
    let decoded = STANDARD.decode(payload).expect("valid base64");
    assert_eq!(String::from_utf8(decoded).expect("utf-8"), text);
}

#[test]
fn empty_text_encodes_an_empty_payload() {
    assert_eq!(osc52_copy(""), b"\x1b]52;c;\x07");
}

#[test]
fn paste_bytes_writes_every_line_break_as_one_return() {
    // Clipboard text from Windows or a browser carries `\r\n`; a paste must
    // send ONE Enter per line break, never two.
    assert_eq!(paste_bytes("a\r\nb", false), b"a\rb");
    assert_eq!(paste_bytes("a\nb", false), b"a\rb");
    assert_eq!(paste_bytes("a\rb", false), b"a\rb");
}
