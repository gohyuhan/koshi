//! Provides the plugin host modules and plugin domain error.
//!
//! The crate defines [`error::PluginError`]. The `types` and `host` modules
//! define no items. Plugin installation and removal state lives in
//! `koshi-plugin-manager`.
//!
//! `cargo xtask dep-guard` permits direct `wasmtime` dependencies only in this
//! crate.

pub mod error;

pub mod types;

pub mod host;
