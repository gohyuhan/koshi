//! Reading the config files at startup.
//!
//! Discovers the config directory and reads the per-section files —
//! `koshi.kdl` (app settings), the color theme `koshi.kdl` names,
//! `keybinding.kdl` (key bindings) — parsing each into its override layer for
//! the runtime to apply. This is the file I/O half the parsers deliberately
//! leave out; the runtime's reload transactions own turning a parsed layer
//! into live state.
//!
//! Themes are a folder, not a file: each one is a `themes/<name>.kdl`, and
//! `koshi.kdl`'s `theme "<name>"` line picks which. The name `default`, a
//! missing line, and a name whose file cannot be loaded all leave koshi's
//! built-in colors in place.
//!
//! A file that is absent, unreadable, or fails to parse is skipped and leaves
//! the built-in defaults in place. `koshi.kdl` and the theme file are
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
use std::io;
use std::path::Path;

use koshi_config::app_config::{parse_app_config, AppConfigFile};
use koshi_config::keybinding::parse_keybindings;
use koshi_config::layer::{PartialKeybindingsConfig, PartialKoshiConfig, PartialThemeConfig};
use koshi_config::profile::parse_profile;
use koshi_config::theme::parse_theme;
use koshi_config::types::DEFAULT_THEME;
use koshi_layout::template::ProfileTemplate;

#[cfg(test)]
mod tests;

/// The user's parsed config layers, each `None` when its file is absent or
/// could not be loaded.
#[derive(Debug, Default)]
pub struct LoadedConfig {
    /// The `koshi.kdl` app-settings layer.
    pub app: Option<PartialKoshiConfig>,
    /// The layer of the `themes/<name>.kdl` `koshi.kdl` selected.
    pub theme: Option<PartialThemeConfig>,
    /// The `keybinding.kdl` layer.
    pub keybindings: Option<PartialKeybindingsConfig>,
}

/// Read and parse the config files from the config directory. Missing,
/// unreadable, or unparseable files are skipped and leave the defaults in
/// place.
///
/// Returns the parsed layers together with a warning per skip, in file order
/// (`koshi.kdl`, then the theme it names, then `keybinding.kdl`). The caller
/// replays the warnings through the log once the tracing subscriber is up,
/// since this runs before tracing is initialized.
#[must_use]
pub fn load() -> (LoadedConfig, Vec<String>) {
    let mut warnings = Vec::new();
    let Some(dir) = koshi_paths::config_dir() else {
        warnings.push("no config directory found; using built-in defaults".to_string());
        return (LoadedConfig::default(), warnings);
    };
    // `koshi.kdl` names the theme, so it is read first and the name it carries
    // decides which theme file — if any — is read next.
    let (app, selected) = match load_app(&dir.join("koshi.kdl"), &mut warnings) {
        Some(file) => (Some(file.layer), file.theme),
        None => (None, None),
    };
    let loaded = LoadedConfig {
        app,
        theme: selected.and_then(|name| load_theme(&dir, &name, &mut warnings)),
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

/// Parses `koshi.kdl` into its override layer and the theme it names,
/// recording every field-partial skip and dropping the file to defaults on a
/// hard error (bad syntax, unknown version, bad `update`).
fn load_app(path: &Path, warnings: &mut Vec<String>) -> Option<AppConfigFile> {
    let source = read(path, warnings)?;
    match parse_app_config(path, &source) {
        Ok(file) => {
            push_field_warnings(path, &file.warnings, warnings);
            Some(file)
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

/// Parses the theme `name` selects — `themes/<name>.kdl` under `dir` — into
/// its color layer, naming the layer after the file it came from and recording
/// every field-partial skip.
///
/// Returns `None`, which leaves koshi's built-in colors in place, when `name`
/// is [`DEFAULT_THEME`], is not a plain file name, or names a file that is
/// absent, unreadable, or fails to parse. Every one of those but the first is
/// recorded in `warnings`: asking for the built-in theme by name is a normal
/// choice, while asking for a theme koshi could not load is a surprise the
/// user should see explained.
///
/// Unlike the files loaded by [`read`], the theme file is opened directly and
/// its absence reported: `koshi.kdl` and `keybinding.kdl` are optional, so
/// missing is silent and normal there, but a theme koshi was *told* to use and
/// cannot find is worth a line in the log.
fn load_theme(dir: &Path, name: &str, warnings: &mut Vec<String>) -> Option<PartialThemeConfig> {
    if name == DEFAULT_THEME {
        return None;
    }
    // A theme name is a single file stem under `themes/`, held to the same
    // rule as a profile name so `theme "../../secret"` cannot read a `.kdl`
    // outside the theme directory.
    if !is_plain_file_name(name) {
        return fall_back_to_default(
            warnings,
            format!("theme name `{name}` must be a plain name"),
        );
    }
    let path = dir.join("themes").join(format!("{name}.kdl"));
    // Read once and take the reason off the error, rather than asking whether
    // the file exists and then opening it: a named theme that is absent and one
    // that is unreadable get different warnings, and reading once means the
    // warning always matches what actually happened.
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return fall_back_to_default(
                warnings,
                format!("theme `{name}` not found at {}", path.display()),
            );
        }
        Err(err) => {
            return fall_back_to_default(
                warnings,
                format!(
                    "theme `{name}` could not be read ({}): {err}",
                    path.display()
                ),
            );
        }
    };
    match parse_theme(&path, &source) {
        Ok((mut layer, field_warnings)) => {
            push_field_warnings(&path, &field_warnings, warnings);
            // The file carries no name of its own; it is the theme `name`
            // asked for, so the layer is labelled with what was selected.
            layer.name = Some(name.to_string());
            Some(layer)
        }
        Err(err) => fall_back_to_default(
            warnings,
            format!("theme `{name}` not applied ({}): {err}", path.display()),
        ),
    }
}

/// Records `reason` as the warning for a theme that could not be used, saying
/// which theme stands instead, and yields the `None` that leaves the built-in
/// colors in place.
///
/// Every theme failure ends here, so all of them read alike: a
/// `theme "../../x"` gives "theme name `../../x` must be a plain name; using
/// the default theme".
fn fall_back_to_default(warnings: &mut Vec<String>, reason: String) -> Option<PartialThemeConfig> {
    warnings.push(format!("{reason}; using the {DEFAULT_THEME} theme"));
    None
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
    if !is_plain_file_name(name) {
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

/// Whether `name` is exactly its own file name — no separators, no root or
/// prefix, no `.`/`..`, not empty — so a `<dir>/<name>.kdl` a config file names
/// stays a flat file directly under `<dir>`, never a nested path or one that
/// escapes it. A name whose final component differs from the whole string
/// (`../x`, `a/b`, `/etc/x`, `foo/`) is not a plain name.
///
/// Both name-selected config files are held to this: the `--profile <name>`
/// under `profile/` and the `theme "<name>"` under `themes/`.
fn is_plain_file_name(name: &str) -> bool {
    Path::new(name).file_name().and_then(|file| file.to_str()) == Some(name)
}
