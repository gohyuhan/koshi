//! Bytes written as text, two ways.
//!
//! **Base64, for a field on the wire.** A `Vec<u8>` field marked
//! `#[serde(with = "crate::bytes")]` travels as a JSON string: base64 as RFC
//! 4648 section 4 spells it, over the alphabet `A`-`Z`, `a`-`z`, `0`-`9`, `+`
//! and `/`, padded with `=` to a multiple of four characters. Decoding refuses
//! anything else — a last group of one character, wrong padding, a character
//! the alphabet does not allow where it stands, or a last character carrying
//! unused bits that are not zero.
//!
//! Example — the two bytes `[104, 105]`:
//!
//! ```text
//! "bytes":"aGk="
//! ```
//!
//! **Hex, for a secret and for a fingerprint.** [`hex()`](crate::bytes::hex)
//! writes bytes as lowercase hex. Every secret, hash and certificate
//! fingerprint koshi holds is written this way. Example — the two bytes
//! `[104, 105]` become `"6869"`.

use std::fmt;

use base64::engine::general_purpose::STANDARD;
use base64::{DecodeError, Engine as _};
use serde::de::{Error, Visitor};
use serde::{Deserializer, Serializer};

/// Write `bytes` as lowercase hex, two characters per byte.
///
/// Example — `[104, 105]` becomes `"6869"`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// Write `bytes` as one base64 string.
///
/// # Errors
/// Returns whatever `serializer` reports for a string it cannot write.
pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&encode(bytes))
}

/// Read one base64 string back into the bytes it holds.
///
/// # Errors
/// Returns a decoding error when the value is not a string, and one naming
/// the fault when the string is not base64: a last group of one character,
/// wrong padding, a last character carrying unused bits that are not zero,
/// or a character the alphabet does not allow where it stands.
pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_str(Base64Visitor)
}

/// Takes the base64 string [`deserialize`] asks for.
struct Base64Visitor;

impl Visitor<'_> for Base64Visitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("bytes as a base64 string")
    }

    fn visit_str<E>(self, text: &str) -> Result<Vec<u8>, E>
    where
        E: Error,
    {
        decode(text).map_err(E::custom)
    }
}

/// `bytes` as base64 text.
fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// The bytes `text` holds, or the reason it is not base64: a last group of
/// one character, wrong padding, a last character carrying unused bits that
/// are not zero, or a character the alphabet does not allow where it stands.
fn decode(text: &str) -> Result<Vec<u8>, &'static str> {
    STANDARD.decode(text).map_err(|error| match error {
        DecodeError::InvalidLength(_) => "the base64 text length is not a multiple of four",
        DecodeError::InvalidPadding => {
            "the base64 text is not padded to a multiple of four characters"
        }
        DecodeError::InvalidLastSymbol(_, _) => {
            "the base64 text ends with unused bits that are not zero"
        }
        DecodeError::InvalidByte(_, _) => {
            "the base64 text holds a character the alphabet does not allow there"
        }
    })
}

#[cfg(test)]
mod tests;
