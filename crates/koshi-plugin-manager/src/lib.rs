//! Plugin lifecycle manager for koshi.
//!
//! This crate covers install, uninstall, enable, disable, update, list, metadata
//! index, lockfile, registry resolution, local file sources, integrity checks,
//! and plugin store layout. This crate owns the plugin inventory state.
//!
//! This crate must not depend on `koshi-runtime` or `koshi-ipc`.

pub mod error;
pub mod types;

pub mod manager;
