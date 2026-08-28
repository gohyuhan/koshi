//! Tests for OSC 133 marker parsing.

use super::*;

#[test]
fn osc133_parses_each_marker_and_exit_code() {
    assert_eq!(parse_osc133(&[b"133", b"A"]), Some(Osc133::Prompt));
    assert_eq!(parse_osc133(&[b"133", b"B"]), Some(Osc133::Input));
    assert_eq!(parse_osc133(&[b"133", b"C"]), Some(Osc133::CommandStart));
    assert_eq!(
        parse_osc133(&[b"133", b"D"]),
        Some(Osc133::CommandFinished(None))
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"0"]),
        Some(Osc133::CommandFinished(Some(0)))
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"137"]),
        Some(Osc133::CommandFinished(Some(137)))
    );
}

#[test]
fn osc133_rejects_unrelated_and_malformed_payloads() {
    assert_invalid(&[b"7", b"A"]);
    assert_invalid(&[b"133"]);
    assert_invalid(&[b"133", b"E"]);
    assert_invalid(&[b"133", b"A", b"extra"]);
    assert_invalid(&[b"133", b"D", b""]);
    assert_invalid(&[b"133", b"D", b"not-a-number"]);
    assert_invalid(&[b"133", b"D", b"0", b"extra"]);
}

fn assert_invalid(params: &[&[u8]]) {
    assert_eq!(parse_osc133(params), None, "payload {params:?}");
}
