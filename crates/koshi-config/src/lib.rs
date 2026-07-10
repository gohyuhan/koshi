//! `koshi-config` — koshi's configuration system: the typed config schema and
//! its built-in defaults, folding user override layers onto those defaults,
//! parsing keybinding chord and leader syntax, and reporting config parse and
//! validation errors. Discovering and reading config files from disk, full
//! schema validation, and migrating older config files forward are also part
//! of this system.

pub mod error;
pub mod key;
pub mod layer;
pub mod parser;
pub mod types;

pub mod config;
