//! Sixel host capability requests.

/// Bytes that request primary device attributes (`ESC[c`).
pub const PRIMARY_DEVICE_ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";

/// Bytes that request the terminal's configured Sixel palette capacity
/// (`ESC[?1;1;0S`).
pub const SIXEL_PALETTE_QUERY: &[u8] = b"\x1b[?1;1;0S";

/// Bytes that request the terminal's configured maximum Sixel geometry
/// (`ESC[?2;4;0S`).
pub const SIXEL_GEOMETRY_QUERY: &[u8] = b"\x1b[?2;4;0S";

#[cfg(test)]
mod tests;
