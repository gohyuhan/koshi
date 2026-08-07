//! `koshi-config` — koshi's configuration system.
//!
//! What lives here: the typed config schema and its built-in defaults
//! ([`types`]), folding user override layers onto them ([`layer`]), the
//! keybinding chord/sequence/leader syntax ([`key`], [`key_sequence`]),
//! keybinding files parsed into keymap layers ([`keybinding`]), conflict
//! detection across those layers ([`conflict`]), merging them into the
//! per-mode lookup tables ([`keymap_merge`]), profile files parsed into
//! templates ([`profile`]), and the config error types ([`error`]).
//!
//! The other modules serve those. [`parser`] holds the KDL entry point and
//! the field readers the file parsers share. [`app_config`] parses
//! `koshi.kdl`. [`theme`] parses one `themes/<name>.kdl`. [`migration`]
//! validates a versioned file and moves it forward to the current schema.
//! [`hints`] resolves the merged keymap into the table the hint bar reads.
//! [`config`] is a placeholder for the standard source layout.
//!
//! No module here reads a file. The caller reads the text and passes it in.

pub mod app_config;
pub mod conflict;
pub mod error;
pub mod hints;
pub mod key;
pub mod key_sequence;
pub mod keybinding;
pub mod keymap_merge;
pub mod layer;
pub mod migration;
pub mod parser;
pub mod profile;
pub mod theme;
pub mod types;

pub mod config;
