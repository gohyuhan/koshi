//! Reading the config files at startup.
//!
//! Discovers the config directory and reads the three per-section files —
//! `koshi.kdl` (app settings), `theme.kdl` (colors), `keybinding.kdl` (key
//! bindings) — parsing each into its override layer for the runtime to apply.
//! This is the file I/O half the parsers deliberately leave out; the runtime's
//! reload transactions own turning a parsed layer into live state.
//!
//! A file that is absent, unreadable, or fails to parse is skipped with a log
//! line and leaves the built-in defaults in place. `koshi.kdl` and `theme.kdl`
//! are field-partial, so a single bad field is logged and the rest of the file
//! still applies; `keybinding.kdl` is all-or-nothing, so any parse error drops
//! the whole file to defaults (a conflict in a file that *parses* is caught
//! later, when the runtime applies it).

use std::fs;
use std::path::Path;

use koshi_config::app_config::parse_app_config;
use koshi_config::keybinding::parse_keybindings;
use koshi_config::layer::{PartialKeybindingsConfig, PartialKoshiConfig, PartialThemeConfig};
use koshi_config::profile::parse_profile;
use koshi_config::theme::parse_theme;
use koshi_layout::template::ProfileTemplate;

/// The user's parsed config layers, each `None` when its file is absent or
/// could not be loaded.
#[derive(Debug, Default)]
pub struct LoadedConfig {
    /// The `koshi.kdl` app-settings layer.
    pub app: Option<PartialKoshiConfig>,
    /// The `theme.kdl` layer.
    pub theme: Option<PartialThemeConfig>,
    /// The `keybinding.kdl` layer.
    pub keybindings: Option<PartialKeybindingsConfig>,
}

/// Read and parse the three config files from the config directory. Missing,
/// unreadable, or unparseable files are skipped and leave the defaults in
/// place.
#[must_use]
pub fn load() -> LoadedConfig {
    let Some(dir) = koshi_paths::config_dir() else {
        tracing::warn!("no config directory found; using built-in defaults");
        return LoadedConfig::default();
    };
    LoadedConfig {
        app: load_app(&dir.join("koshi.kdl")),
        theme: load_theme(&dir.join("theme.kdl")),
        keybindings: load_keybindings(&dir.join("keybinding.kdl")),
    }
}

/// The file's text, or `None` when it is absent (not an error) or unreadable.
fn read(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    match fs::read_to_string(path) {
        Ok(source) => Some(source),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "could not read config file");
            None
        }
    }
}

/// Parses `koshi.kdl`, logging every field-partial skip and dropping the file
/// to defaults on a hard error (bad syntax, unknown version, bad `update`).
fn load_app(path: &Path) -> Option<PartialKoshiConfig> {
    let source = read(path)?;
    match parse_app_config(path, &source) {
        Ok((layer, warnings)) => {
            log_field_warnings(path, &warnings);
            Some(layer)
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "koshi.kdl not applied; using defaults");
            None
        }
    }
}

/// Parses `theme.kdl`, logging every field-partial skip and dropping the file
/// to defaults on a hard error.
fn load_theme(path: &Path) -> Option<PartialThemeConfig> {
    let source = read(path)?;
    match parse_theme(path, &source) {
        Ok((layer, warnings)) => {
            log_field_warnings(path, &warnings);
            Some(layer)
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "theme.kdl not applied; using defaults");
            None
        }
    }
}

/// Parses `keybinding.kdl` all-or-nothing: any parse error drops the whole file.
fn load_keybindings(path: &Path) -> Option<PartialKeybindingsConfig> {
    let source = read(path)?;
    match parse_keybindings(path, &source) {
        Ok(partial) => Some(partial),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "keybinding.kdl not applied; using defaults");
            None
        }
    }
}

/// Logs each field-partial skip from a parsed file.
fn log_field_warnings(path: &Path, warnings: &[String]) {
    for warning in warnings {
        tracing::warn!(path = %path.display(), "{warning}");
    }
}

/// Read and parse `profile/<name>.kdl` from the config directory. A missing,
/// unreadable, or invalid profile is logged and returns `None`; the caller then
/// starts a single shell. Profiles are all-or-nothing: any schema violation
/// drops the whole file, since a half-applied profile would spawn some of its
/// panes and silently omit others.
#[must_use]
pub fn load_profile(name: &str) -> Option<ProfileTemplate> {
    let dir = koshi_paths::config_dir()?;
    let path = dir.join("profile").join(format!("{name}.kdl"));
    if !path.exists() {
        tracing::warn!(path = %path.display(), "profile `{name}` not found; starting a single shell");
        return None;
    }
    let source = read(&path)?;
    match parse_profile(&path, &source) {
        Ok(template) => Some(template),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "profile `{name}` not applied; starting a single shell");
            None
        }
    }
}
