//! Kitty capability queries.

use std::io::Write;

use crate::KittyOutputError;

/// The image number reserved by the Kitty support query.
pub const KITTY_QUERY_IMAGE_ID: u32 = u32::MAX;

/// Write the one-pixel non-storing Kitty support query.
pub fn write_kitty_support_query<W: Write>(writer: &mut W) -> Result<(), KittyOutputError> {
    writer.write_all(b"\x1b_Gi=4294967295,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\")?;
    Ok(())
}

#[cfg(test)]
mod tests;
