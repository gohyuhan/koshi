//! Reading a message whose variant this build may not have.
//!
//! Two koshi builds of different versions share one socket, and the newer one
//! names request kinds, results and events the older one has no name for.
//!
//! [`MaybeKnown`](crate::wire::MaybeKnown) decodes the message, and a message
//! that decodes is [`MaybeKnown::Known`](crate::wire::MaybeKnown::Known). A
//! message that does not decode has its variant name read and compared against
//! [`WireVariants::VARIANTS`](crate::wire::WireVariants::VARIANTS), the names
//! this build has. A name off the list becomes
//! [`MaybeKnown::Unknown`](crate::wire::MaybeKnown::Unknown), which the caller
//! answers and then keeps reading. A name on the list keeps the decoding
//! error. A value that names no variant is the error `a wire value is a
//! variant name, or a one-key object naming one`.
//!
//! Example — a build that has no `Floating` request kind:
//!
//! ```text
//! {"Layout":{"tab":null}}   -> MaybeKnown::Known(IpcRequestKind::Layout { .. })
//! {"Floating":{"pane":3}}   -> MaybeKnown::Unknown { name: "Floating" }
//! {"Layout":{"tab":7.5}}    -> Err: Layout is a name this build has
//! 7                         -> Err: 7 names no variant
//! ```
//!
//! The variant name lives in the JSON the transport speaks: a variant carrying
//! fields is a one-key object whose key is the name, and a variant carrying
//! none is that name as a bare string.
//!
//! An unknown name is what a refusal and a log line quote back, so it is
//! filtered by
//! [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text) as it
//! is read: `{"\u{1b}[2JFloating":{}}` reads as
//! `MaybeKnown::Unknown { name: "[2JFloating" }`, and a name of a million
//! characters is cut to
//! [`MAX_REPORTED_TEXT_BYTES`](koshi_core::text::MAX_REPORTED_TEXT_BYTES).

use std::fmt;

use serde::de::{DeserializeOwned, Error as _, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

/// One message asking a peer to do something, on any of koshi's protocols.
///
/// The envelope's own fields are fixed: decoding rejects any field it does not
/// know, and a misspelled `request_id` is an error. What may travel inside `K`
/// is each protocol's own business.
///
/// `K` is the request kind. A sender uses the protocol's own kind. A server
/// uses [`MaybeKnown<K>`], where a kind this build does not have arrives as
/// [`MaybeKnown::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<K> {
    /// Caller-chosen id, repeated in the answer to this message. Unique among
    /// the messages in flight on one connection.
    pub request_id: u64,
    /// What is being asked.
    pub kind: K,
}

/// One message answering an [`Envelope`], on any of koshi's protocols.
///
/// The envelope's own fields are fixed, the same way [`Envelope`]'s are.
///
/// `R` is the answer. A server uses the protocol's own result. A caller uses
/// [`MaybeKnown<R>`], where a result this build does not have arrives as
/// [`MaybeKnown::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer<R> {
    /// The `request_id` of the message being answered, or `None` when the
    /// bytes received were too malformed to read one — a caller that sent
    /// request 7 and reads `None` knows the answer belongs to no request of
    /// its own.
    pub request_id: Option<u64>,
    /// The answer itself.
    pub result: R,
}

/// The variant names a build can decode, for one wire enum.
///
/// Implemented by hand, one entry per variant. A name absent from the list
/// reads as [`MaybeKnown::Unknown`] when its payload does not decode.
pub trait WireVariants {
    /// Every variant name this build has, spelled as it travels.
    const VARIANTS: &'static [&'static str];
}

/// The variant name one value travels under.
pub trait WireName {
    /// This value's variant name, spelled as it travels.
    fn wire_name(&self) -> &'static str;
}

/// One wire value, which may name a variant this build does not have.
///
/// Decoding fails in two cases: a name this build has carries a payload it
/// cannot read, or the value names no variant at all — a number, an array,
/// `null`, an empty object, or an object with two keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaybeKnown<T> {
    /// The build has this variant, decoded.
    Known(T),
    /// The peer named a variant this build does not have.
    Unknown {
        /// The name the peer spelled, filtered by
        /// [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text):
        /// no control or bidi character, and at most
        /// [`MAX_REPORTED_TEXT_BYTES`](koshi_core::text::MAX_REPORTED_TEXT_BYTES)
        /// bytes. Every variant name this build has is unchanged by that
        /// filter.
        name: String,
    },
}

impl<'de, T> Deserialize<'de> for MaybeKnown<T>
where
    T: DeserializeOwned + WireVariants,
{
    /// The message is held as its raw JSON text and decoded from that text.
    /// The payload is read once when the decode succeeds. After a decode
    /// fails, the variant name is read out of the same text: a name this build
    /// does not have makes the value unknown, and a name it has keeps the
    /// decoding error.
    ///
    /// The raw text is borrowed from the input: decoding works through
    /// `serde_json::from_str` and `serde_json::from_slice`, and fails at run
    /// time through `serde_json::from_reader`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <&RawValue>::deserialize(deserializer)?.get();
        let refusal = match serde_json::from_str(text) {
            Ok(value) => return Ok(MaybeKnown::Known(value)),
            Err(refusal) => refusal,
        };
        let Some(name) = variant_name(text) else {
            return Err(D::Error::custom(
                "a wire value is a variant name, or a one-key object naming one",
            ));
        };
        if T::VARIANTS.contains(&name.as_str()) {
            return Err(D::Error::custom(refusal));
        }
        Ok(MaybeKnown::Unknown {
            name: koshi_core::text::sanitize_reported_text(&name),
        })
    }
}

/// Decode `T`, falling back to `T::default()` when the value cannot be read.
///
/// A presentation value — a color, an underline style, a cursor shape — falls
/// back to its `Default`. Used through
/// `#[serde(default, deserialize_with = "…")]` on the field that holds it.
/// Presentation values only; a request kind or a result uses [`MaybeKnown`].
/// The raw text is borrowed from the input, the same way [`MaybeKnown`]
/// borrows it.
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
/// for anything else, including an object carrying a second key: the
/// deserializer refuses the entries the visitor left unread.
///
/// The payload is stepped over without being decoded.
fn variant_name(text: &str) -> Option<String> {
    serde_json::Deserializer::from_str(text)
        .deserialize_any(NameVisitor)
        .ok()
}

/// Reads the variant name: a bare string is the name, and an object gives its
/// first key. The value beside that key is stepped over.
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
        // `IgnoredAny` walks the payload's syntax and allocates nothing.
        map.next_value::<IgnoredAny>()?;
        Ok(name)
    }
}

#[cfg(test)]
mod tests;
