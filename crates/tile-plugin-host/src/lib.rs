//! `tile-plugin-host` — plugin runtime host: Wasmtime integration, instance
//! lifecycle, permissions enforcement, host functions, plugin panes, and plugin
//! status UI. Sole owner of the `wasmtime` dependency. Executes plugins but does
//! not own install/uninstall state.

pub mod error;
pub mod types;

pub mod host;
