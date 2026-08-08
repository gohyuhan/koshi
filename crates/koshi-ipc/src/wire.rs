//! Reading a message whose variant this build may not have.
//!
//! Two koshi builds of different versions share one socket. The newer one
//! names request kinds, results and events the older one was compiled without.
//! Plain decoding refuses an unknown variant, and the refusal happens at the
//! frame layer, so one unreadable message ends the whole connection.
//!
//! [`MaybeKnown`](crate::wire::MaybeKnown) reads the variant name out of the
//! message first and compares it against
//! [`WireVariants::VARIANTS`](crate::wire::WireVariants::VARIANTS), the names
//! this build has. A name on the list decodes as usual, and any error it
//! produces is a real decoding error. A name off the list becomes
//! [`MaybeKnown::Unknown`](crate::wire::MaybeKnown::Unknown), which the caller
//! answers and then keeps reading.
//!
//! Example — a build that has no `Floating` request kind:
//!
//! ```text
//! {"Layout":{"tab":null}}   -> MaybeKnown::Known(IpcRequestKind::Layout { .. })
//! {"Floating":{"pane":3}}   -> MaybeKnown::Unknown { name: "Floating" }
//! {"Layout":{"tab":7.5}}    -> Err, because Layout is a name this build has
//! ```
//!
//! The variant name lives in the JSON the transport speaks: a variant carrying
//! fields is a one-key object whose key is the name, and a variant carrying
//! none is that name as a bare string.

use std::fmt;

use serde::de::{DeserializeOwned, Error as _, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

/// The variant names a build can decode, for one wire enum.
///
/// Implemented by hand, one entry per variant. A variant added to the enum is
/// added here in the same change, or a peer sending it reads as
/// [`MaybeKnown::Unknown`].
pub trait WireVariants {
    /// Every variant name this build has, spelled as it travels.
    const VARIANTS: &'static [&'static str];
}

/// The name one decoded value travels under, for naming a message without its
/// payload.
pub trait WireName {
    /// This value's variant name.
    fn wire_name(&self) -> &'static str;
}

/// One wire value, which may name a variant this build does not have.
///
/// Decoding fails only when a name this build *does* have carries a payload it
/// cannot read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaybeKnown<T> {
    /// The build has this variant, decoded.
    Known(T),
    /// The peer named a variant this build does not have.
    Unknown {
        /// The name as the peer spelled it, for the refusal sent back.
        name: String,
    },
}

impl<'de, T> Deserialize<'de> for MaybeKnown<T>
where
    T: DeserializeOwned + WireVariants,
{
    /// The message is held as its raw JSON text while the name is read, then
    /// decoded from that same text.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <&RawValue>::deserialize(deserializer)?.get();
        let Some(name) = variant_name(text) else {
            return Err(D::Error::custom(
                "a wire value is a variant name, or a one-key object naming one",
            ));
        };
        if !T::VARIANTS.contains(&name.as_str()) {
            return Ok(MaybeKnown::Unknown { name });
        }
        serde_json::from_str(text)
            .map(MaybeKnown::Known)
            .map_err(D::Error::custom)
    }
}

/// Decode `T`, falling back to `T::default()` when the value cannot be read.
///
/// A presentation value — a color, an underline style, a cursor shape — falls
/// back to its plainest value. Used through
/// `#[serde(default, deserialize_with = "…")]` on the field that holds it.
/// Presentation values only; a request kind or a result uses [`MaybeKnown`].
///
/// Example — a cell whose underline arrives as `"Dotted2"` from a newer koshi
/// draws with no underline, and every other cell in the frame is untouched.
pub fn or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let text = <&RawValue>::deserialize(deserializer)?.get();
    Ok(serde_json::from_str(text).unwrap_or_default())
}

/// The variant name a raw JSON message carries: the string itself for a
/// variant with no fields, or the single key for a variant with them. `None`
/// for anything else, including an object carrying a second key.
///
/// The payload is stepped over, never built.
//
// ponytail: `&RawValue` borrows the input, so every caller must decode from
// bytes or a string. `transport::read_message` uses `serde_json::from_slice`,
// which borrows. A future `from_reader` path fails here at run time, not at
// compile time.
fn variant_name(text: &str) -> Option<String> {
    serde_json::Deserializer::from_str(text)
        .deserialize_any(NameVisitor)
        .ok()
}

/// Reads the variant name: a bare string is the name, and an object gives its
/// key. The value beside that key is stepped over.
struct NameVisitor;

impl<'de> Visitor<'de> for NameVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a variant name, or an object naming one")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<String, A::Error> {
        let name = map
            .next_key::<String>()?
            .ok_or_else(|| A::Error::custom("an object naming a variant has a key"))?;
        // The value is stepped over, never built: `IgnoredAny` walks the
        // payload's syntax and allocates nothing.
        map.next_value::<IgnoredAny>()?;
        Ok(name)
    }
}

#[cfg(test)]
mod tests;
