//! Reading the config files at startup.
//!
//! Discovers the config directory and reads the three per-section files —
//! `koshi.kdl` (app settings), `theme.kdl` (colors), `keybinding.kdl` (key
//! bindings) — parsing each into its override layer for the runtime to apply.
//! This is the file I/O half the parsers deliberately leave out; the runtime's
//! reload transactions own turning a parsed layer into live state.
//!
//! A file that is absent, unreadable, or fails to parse is skipped and leaves
//! the built-in defaults in place. `koshi.kdl` and `theme.kdl` are
//! field-partial, so a single bad field is skipped and the rest of the file
//! still applies; `keybinding.kdl` is all-or-nothing, so any parse error drops
//! the whole file to defaults (a conflict in a file that *parses* is caught
//! later, when the runtime applies it).
//!
//! `load` does not log its own warnings: it runs before the tracing
//! subscriber is installed (so `logging.enabled` can decide whether that
//! subscriber writes a file at all), so it returns each skip reason as a
//! string for the caller to replay once tracing is up.

use std::fs;
use std::path::{Component, Path};

use koshi_config::app_config::parse_app_config;
use koshi_config::keybinding::parse_keybindings;
use koshi_config::layer::{PartialKeybindingsConfig, PartialKoshiConfig, PartialThemeConfig};
use koshi_config::profile::parse_profile;
use koshi_config::theme::parse_theme;
use koshi_layout::template::ProfileTemplate;

#[cfg(test)]
mod tests;

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
///
/// Returns the parsed layers together with a warning per skip, in file order
/// (`koshi.kdl`, then `theme.kdl`, then `keybinding.kdl`). The caller replays
/// the warnings through the log once the tracing subscriber is up, since this
/// runs before tracing is initialized.
#[must_use]
pub fn load() -> (LoadedConfig, Vec<String>) {
    let mut warnings = Vec::new();
    let Some(dir) = koshi_paths::config_dir() else {
        warnings.push("no config directory found; using built-in defaults".to_string());
        return (LoadedConfig::default(), warnings);
    };
    let loaded = LoadedConfig {
        app: load_app(&dir.join("koshi.kdl"), &mut warnings),
        theme: load_theme(&dir.join("theme.kdl"), &mut warnings),
        keybindings: load_keybindings(&dir.join("keybinding.kdl"), &mut warnings),
    };
    (loaded, warnings)
}

/// The file's text, or `None` when it is absent (not an error) or unreadable.
/// A read failure is recorded in `warnings`.
fn read(path: &Path, warnings: &mut Vec<String>) -> Option<String> {
    if !path.exists() {
        return None;
    }
    match fs::read_to_string(path) {
        Ok(source) => Some(source),
        Err(err) => {
            warnings.push(format!(
                "could not read config file {}: {err}",
                path.display()
            ));
            None
        }
    }
}

/// Parses `koshi.kdl`, recording every field-partial skip and dropping the file
/// to defaults on a hard error (bad syntax, unknown version, bad `update`).
fn load_app(path: &Path, warnings: &mut Vec<String>) -> Option<PartialKoshiConfig> {
    let source = read(path, warnings)?;
    match parse_app_config(path, &source) {
        Ok((layer, field_warnings)) => {
            push_field_warnings(path, &field_warnings, warnings);
            Some(layer)
        }
        Err(err) => {
            warnings.push(format!(
                "koshi.kdl not applied ({}): {err}; using defaults",
                path.display()
            ));
            None
        }
    }
}

/// Parses `theme.kdl`, recording every field-partial skip and dropping the file
/// to defaults on a hard error.
fn load_theme(path: &Path, warnings: &mut Vec<String>) -> Option<PartialThemeConfig> {
    let source = read(path, warnings)?;
    match parse_theme(path, &source) {
        Ok((layer, field_warnings)) => {
            push_field_warnings(path, &field_warnings, warnings);
            Some(layer)
        }
        Err(err) => {
            warnings.push(format!(
                "theme.kdl not applied ({}): {err}; using defaults",
                path.display()
            ));
            None
        }
    }
}

/// Parses `keybinding.kdl` all-or-nothing: any parse error drops the whole file.
fn load_keybindings(path: &Path, warnings: &mut Vec<String>) -> Option<PartialKeybindingsConfig> {
    let source = read(path, warnings)?;
    match parse_keybindings(path, &source) {
        Ok(partial) => Some(partial),
        Err(err) => {
            warnings.push(format!(
                "keybinding.kdl not applied ({}): {err}; using defaults",
                path.display()
            ));
            None
        }
    }
}

/// Appends each field-partial skip from a parsed file to `warnings`, prefixed
/// with the file it came from.
fn push_field_warnings(path: &Path, field_warnings: &[String], warnings: &mut Vec<String>) {
    for warning in field_warnings {
        warnings.push(format!("{}: {warning}", path.display()));
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
    // A profile name is a single file stem under `profile/`. Reject anything
    // that is not one plain path component — an absolute path, a `..`, or an
    // embedded separator — so `--profile ../secret` or `--profile /etc/x`
    // cannot read a `.kdl` outside the profile directory.
    if !is_plain_profile_name(name) {
        tracing::warn!("profile name `{name}` must be a plain name; starting a single shell");
        return None;
    }
    let path = dir.join("profile").join(format!("{name}.kdl"));
    if !path.exists() {
        tracing::warn!(path = %path.display(), "profile `{name}` not found; starting a single shell");
        return None;
    }
    // Genesis runs after tracing is up, so this path logs directly rather than
    // returning warnings the way [`load`] does.
    let mut warnings = Vec::new();
    let source = read(&path, &mut warnings);
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    let source = source?;
    match parse_profile(&path, &source) {
        Ok(template) => Some(template),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "profile `{name}` not applied; starting a single shell");
            None
        }
    }
}

/// Whether `name` is a single plain path component — no separators, no root or
/// prefix, no `.`/`..` — so joining it under `profile/` cannot escape that
/// directory.
fn is_plain_profile_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}
