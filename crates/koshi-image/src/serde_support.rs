//! Bounded serde visitors for image and graphics byte fields.

use serde::de::{self, DeserializeSeed, SeqAccess, Visitor};
use serde::Deserializer;

/// Deserialize a byte sequence with a maximum length and error label.
///
/// A sequence longer than `limit` returns a deserializer error before the extra byte is stored.
pub struct BoundedBytesSeed {
    limit: usize,
    name: &'static str,
}

impl BoundedBytesSeed {
    /// Create a byte-sequence deserializer with `limit` bytes and the supplied error `name`.
    #[must_use]
    pub const fn new(limit: usize, name: &'static str) -> Self {
        BoundedBytesSeed { limit, name }
    }
}

impl<'de> DeserializeSeed<'de> for BoundedBytesSeed {
    type Value = Vec<u8>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedBytesVisitor {
            limit: self.limit,
            name: self.name,
        })
    }
}

struct BoundedBytesVisitor {
    limit: usize,
    name: &'static str,
}

impl BoundedBytesVisitor {
    fn validate_length<E>(&self, length: usize) -> Result<(), E>
    where
        E: de::Error,
    {
        if length > self.limit {
            return Err(de::Error::custom(format!(
                "{name} exceeds {limit} bytes",
                name = self.name,
                limit = self.limit,
            )));
        }
        Ok(())
    }
}

impl<'de> Visitor<'de> for BoundedBytesVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded byte sequence")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut bytes = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(self.limit));
        while let Some(byte) = sequence.next_element::<u8>()? {
            if bytes.len() == self.limit {
                return Err(de::Error::custom(format!(
                    "{name} exceeds {limit} bytes",
                    name = self.name,
                    limit = self.limit,
                )));
            }
            bytes.push(byte);
        }
        Ok(bytes)
    }

    fn visit_bytes<E>(self, bytes: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.validate_length(bytes.len())?;
        Ok(bytes.to_vec())
    }

    fn visit_byte_buf<E>(self, bytes: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.validate_length(bytes.len())?;
        Ok(bytes)
    }
}
