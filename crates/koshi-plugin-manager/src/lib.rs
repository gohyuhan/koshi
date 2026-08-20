//! Plugin lifecycle manager for koshi.
//!
//! Scope: the plugin inventory state — install, uninstall, enable, disable,
//! update, list, metadata index, lockfile, registry resolution, local file
//! sources, integrity checks, and plugin store layout. Every module is empty.
//!
//! `cargo xtask dep-guard` fails if this crate declares `koshi-runtime`,
//! `koshi-ipc`, or `koshi-plugin-host` as a direct dependency.

pub mod error;
pub mod types;

pub mod manager;
