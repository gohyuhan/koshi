//! A random number for a choice that needs no secrecy: the start of a name
//! walk, or the first id a counter hands out.

use std::hash::{BuildHasher, Hasher, RandomState};

/// A random `u64`: the hash of no bytes under a fresh [`RandomState`], which
/// the standard library builds with random keys. Not for secrets.
#[must_use]
pub fn generate_random_number() -> u64 {
    RandomState::new().build_hasher().finish()
}
