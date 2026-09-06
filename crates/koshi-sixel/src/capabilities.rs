//! Sixel host capability requests.

/// Request primary device attributes from the terminal host.
pub const PRIMARY_DEVICE_ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";

/// Request the terminal's configured Sixel palette capacity.
pub const SIXEL_PALETTE_QUERY: &[u8] = b"\x1b[?1;1;0S";

/// Request the terminal's configured maximum Sixel geometry.
pub const SIXEL_GEOMETRY_QUERY: &[u8] = b"\x1b[?2;4;0S";

#[cfg(test)]
mod tests;
