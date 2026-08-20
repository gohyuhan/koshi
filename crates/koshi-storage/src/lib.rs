//! `koshi-storage` — atomic file replacement: a write lands whole or not at
//! all, so a reader finds the old file or the new one, never a torn middle.
//! [`error::StorageError`] carries every failure the crate reports.

pub mod atomic;
pub mod error;
pub mod types;

pub mod store;
