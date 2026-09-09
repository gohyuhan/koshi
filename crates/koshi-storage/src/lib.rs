//! `koshi-storage` provides atomic file replacement: readers see the complete
//! old file or the complete new file, never a torn middle.
//! [`error::StorageError`] carries every reported failure.

pub mod atomic;
pub mod error;
