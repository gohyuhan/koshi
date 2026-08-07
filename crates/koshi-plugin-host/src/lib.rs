//! `koshi-plugin-host` — plugin runtime host: Wasmtime integration, instance
//! lifecycle, permissions enforcement, host functions, plugin panes, and plugin
//! status UI. Sole owner of the `wasmtime` dependency. Runs plugins. Does not
//! own install or uninstall state.

/// Plugin load and run failures.
pub mod error;

/// Shared types for the plugin host.
pub mod types;

/// Plugin instance lifecycle, event handling, and state.
pub mod host;
