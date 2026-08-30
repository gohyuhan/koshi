//! `koshi-storage` — atomic file replacement. A reader finds the whole old
//! file or the whole new one, never a torn middle.
//! [`error::StorageError`] carries every failure the crate reports.

pub mod atomic;
pub mod error;
