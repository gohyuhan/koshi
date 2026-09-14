//! Bounded serde visitors for image and graphics byte fields.

use serde::de::{self, DeserializeSeed, SeqAccess, Visitor};
use serde::Deserializer;

/// Deserialize a byte sequence with a maximum length and error label.
///
/// A sequence longer than `limit` returns a deserializer error before the extra byte is stored.
pub struct BoundedBytesSeed {
    byte_limit: usize,
    error_label: &'static str,
}

impl BoundedBytesSeed {
    /// Create a byte-sequence deserializer with `byte_limit` bytes and the supplied error label.
    #[must_use]
    pub const fn from_byte_limit_and_error_label(
        byte_limit: usize,
        error_label: &'static str,
    ) -> Self {
        BoundedBytesSeed {
            byte_limit,
            error_label,
        }
    }
}

impl<'de> DeserializeSeed<'de> for BoundedBytesSeed {
    type Value = Vec<u8>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedBytesVisitor {
            byte_limit: self.byte_limit,
            error_label: self.error_label,
        })
    }
}

struct BoundedBytesVisitor {
    byte_limit: usize,
    error_label: &'static str,
}

impl BoundedBytesVisitor {
    fn validate_byte_count<E>(&self, byte_count: usize) -> Result<(), E>
    where
        E: de::Error,
    {
        if byte_count > self.byte_limit {
            return Err(de::Error::custom(format!(
                "{error_label} exceeds {byte_limit} bytes",
                error_label = self.error_label,
                byte_limit = self.byte_limit,
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

    fn visit_seq<A>(self, mut byte_sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut byte_values =
            Vec::with_capacity(byte_sequence.size_hint().unwrap_or(0).min(self.byte_limit));
        while let Some(byte_value) = byte_sequence.next_element::<u8>()? {
            if byte_values.len() == self.byte_limit {
                return Err(de::Error::custom(format!(
                    "{error_label} exceeds {byte_limit} bytes",
                    error_label = self.error_label,
                    byte_limit = self.byte_limit,
                )));
            }
            byte_values.push(byte_value);
        }
        Ok(byte_values)
    }

    fn visit_bytes<E>(self, byte_values: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.validate_byte_count(byte_values.len())?;
        Ok(byte_values.to_vec())
    }

    fn visit_byte_buf<E>(self, byte_values: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.validate_byte_count(byte_values.len())?;
        Ok(byte_values)
    }
}
