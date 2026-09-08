//! Contract tests for Sixel host capability requests.

use super::*;

#[test]
fn uses_primary_device_attributes_request() {
    assert_eq!(PRIMARY_DEVICE_ATTRIBUTES_QUERY, b"\x1b[c");
}

#[test]
fn uses_xterm_sixel_palette_and_geometry_requests() {
    assert_eq!(SIXEL_PALETTE_QUERY, b"\x1b[?1;1;0S");
    assert_eq!(SIXEL_GEOMETRY_QUERY, b"\x1b[?2;4;0S");
}
