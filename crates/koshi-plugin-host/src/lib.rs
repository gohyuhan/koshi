//! Plugin runtime host for koshi.
//!
//! Scope: Wasmtime integration, instance lifecycle, permissions enforcement,
//! host functions, plugin panes, and plugin status UI. Install and uninstall
//! state lives in `koshi-plugin-manager`. Only [`error::PluginError`] exists;
//! every other module is empty.
//!
//! `cargo xtask dep-guard` fails if any crate other than this one declares
//! `wasmtime` as a direct dependency.

pub mod error;

pub mod types;

pub mod host;
