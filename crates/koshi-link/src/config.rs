//! Reading the config files at startup.
//!
//! Every loader reads the config directory its caller names. A process entry
//! resolves that directory through `koshi_paths::resolve_config_directory`
//! and passes it down; `None` reads no file and leaves every built-in
//! default in place. Reads the per-section files —
//! `koshi.kdl` (app settings), the color theme `koshi.kdl` names,
//! `keybinding.kdl` (key bindings) — parsing each into its override layer. The
//! runtime's reload transactions turn a parsed layer into live state.
//!
//! Themes are a folder, not a file: each one is a `themes/<name>.kdl`, and
//! `koshi.kdl`'s `theme "<name>"` line picks which. The name `default`, a
//! missing line, and a name whose file cannot be loaded all leave koshi's
//! built-in colors in place.
//!
//! A file that is absent, unreadable, or fails to parse is skipped and leaves
//! the built-in defaults in place. `koshi.kdl` and the theme file are
//! field-partial: a single bad field is skipped and the rest of the file still
//! applies. `keybinding.kdl` is all-or-nothing: any parse error drops the whole
//! file to defaults. A conflict in a `keybinding.kdl` that *parses* is caught
//! where the runtime applies it, not here.
//!
//! `load_config_files` writes no log line of its own. It runs before the
//! tracing subscriber is installed, and returns each skip reason as a string
//! the caller replays once tracing is up.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use koshi_config::app_config::{parse_app_config, AppConfigFile};
use koshi_config::keybinding::parse_keybindings;
use koshi_config::layer::{
    merge_client, merge_server, PartialKeybindingsConfig, PartialKoshiConfig, PartialThemeConfig,
};
use koshi_config::profile::parse_profile;
use koshi_config::theme::parse_theme;
use koshi_config::types::{ClientConfig, ServerConfig, DEFAULT_THEME_NAME};
use koshi_core::geometry::Direction;
use koshi_core::ids::SessionId;
use koshi_layout::template::ProfileTemplate;
use koshi_observability::logging::LoggingParameters;
use koshi_runtime::ipc_server::{OtherUsers, OtherUsersSetting};

#[cfg(test)]
mod tests;

/// The user's parsed config layers, each `None` when its file is absent or
/// could not be loaded.
#[derive(Debug, Default)]
pub struct LoadedConfig {
    /// The `koshi.kdl` app-settings layer.
    pub app_config_layer: Option<PartialKoshiConfig>,
    /// The layer of the `themes/<name>.kdl` `koshi.kdl` selected.
    pub theme_config_layer: Option<PartialThemeConfig>,
    /// The `keybinding.kdl` layer.
    pub keybindings_config_layer: Option<PartialKeybindingsConfig>,
}

/// Read and parse the config files from the config directory. Missing,
/// unreadable, or unparseable files are skipped and leave the defaults in
/// place.
///
/// Returns the parsed layers together with a warning per skip, in file order
/// (`koshi.kdl`, then the theme it names, then `keybinding.kdl`). The caller
/// replays the warnings through the log once the tracing subscriber is up.
///
/// A `config_directory` of `None` gives every layer `None` and the one
/// warning `"no config directory found; using built-in defaults"`.
#[must_use]
pub fn load_config_files(config_directory: Option<&Path>) -> (LoadedConfig, Vec<String>) {
    let mut config_warnings = Vec::new();
    let Some(config_directory) = config_directory else {
        config_warnings.push("no config directory found; using built-in defaults".to_string());
        return (LoadedConfig::default(), config_warnings);
    };
    // `koshi.kdl` is read first. The theme name it carries picks which theme
    // file — if any — is read next.
    let (app_config_layer, selected_theme_name) =
        match load_app_config(&config_directory.join("koshi.kdl"), &mut config_warnings) {
            Some(app_config_file) => (Some(app_config_file.layer), app_config_file.theme_name),
            None => (None, None),
        };
    let loaded_config = LoadedConfig {
        app_config_layer,
        theme_config_layer: selected_theme_name.and_then(|theme_name| {
            load_theme_config(config_directory, &theme_name, &mut config_warnings)
        }),
        keybindings_config_layer: load_keybindings_config(
            &config_directory.join("keybinding.kdl"),
            &mut config_warnings,
        ),
    };
    (loaded_config, config_warnings)
}

/// Read and parse `koshi.kdl` in `config_directory` alone, skipping the theme
/// and the keymap.
///
/// `koshi.kdl` is the only file carrying the top-level `allow-beta-features`
/// and `layout.new-pane-direction`: a `koshi new-pane` reads that file and
/// nothing else. A `config_directory` of `None`, and a file that is absent,
/// unreadable, or unparseable, yield `None`, which folds to the built-in
/// defaults. Warnings are dropped.
#[must_use]
pub fn load_app_layer(config_directory: Option<&Path>) -> Option<PartialKoshiConfig> {
    let config_directory = config_directory?;
    let mut config_warnings = Vec::new();
    load_app_config(&config_directory.join("koshi.kdl"), &mut config_warnings)
        .map(|app_config_file| app_config_file.layer)
}

/// The tracing subscriber's settings for `session_id`: `app_config_layer`'s
/// `logging` section over the built-in defaults. The session server and every
/// client attached to it build their parameters here; one session's lines all
/// land in one file.
#[must_use]
pub fn build_logging_parameters(
    app_config_layer: Option<&PartialKoshiConfig>,
    session_id: SessionId,
) -> LoggingParameters {
    let logging_config = app_config_layer
        .map(PartialKoshiConfig::get_logging_config)
        .unwrap_or_default();
    LoggingParameters {
        is_enabled: logging_config.is_enabled,
        log_level: logging_config.log_level,
        log_format: logging_config.log_format,
        session_id,
    }
}

/// What the session's control socket needs to serve the other users of this
/// machine, or `None` when only the user who started the session may reach it.
///
/// `is_other_user_access_forced` is the `--allow-other-users` flag: `true`
/// serves them whatever `koshi.kdl` says, and `false` leaves the answer to
/// that file's `allow-other-users`.
///
/// A forced switch stays on for the session's whole life. A switch left to the
/// file is read again from `koshi.kdl` in `config_directory` on every request
/// from another user: an `allow-other-users` turned off after the session
/// started closes the connections it had admitted.
///
/// The socket's directory is `koshi.kdl`'s `shared-sessions-dir` when it names
/// one, and the platform's machine-wide location otherwise. No directory from
/// either — Windows reporting no `ProgramData` — serves only this user.
#[must_use]
pub fn resolve_other_users_policy(
    app_config_layer: Option<&PartialKoshiConfig>,
    is_other_user_access_forced: bool,
    config_directory: Option<&Path>,
) -> Option<OtherUsers> {
    let server_config = merge_server(
        ServerConfig::default(),
        app_config_layer.cloned().into_iter().collect(),
    );
    if !is_other_user_access_forced && !server_config.should_allow_other_users {
        return None;
    }
    let shared_sessions_directory = resolve_shared_sessions_directory(&server_config)?;
    let other_user_access_check: OtherUsersSetting = if is_other_user_access_forced {
        Arc::new(|| true)
    } else {
        let config_directory = config_directory.map(Path::to_path_buf);
        Arc::new(move || is_other_user_access_allowed(config_directory.as_deref()))
    };
    Some(OtherUsers {
        shared_directory: shared_sessions_directory,
        is_enabled: other_user_access_check,
    })
}

/// The machine-wide directory the sessions of one user are advertised in:
/// `server_config`'s `shared-sessions-dir` when it names one, and the
/// platform's own machine-wide location otherwise. `None` when neither names
/// one — Windows reporting no `ProgramData`.
///
/// The session server creates its socket here, and a `koshi` command looks
/// here for the sessions the other local users started.
pub fn resolve_shared_sessions_directory(server_config: &ServerConfig) -> Option<PathBuf> {
    server_config
        .shared_sessions_directory
        .clone()
        .or_else(koshi_paths::resolve_shared_sessions_directory)
}

/// The `server` settings `koshi.kdl` in `config_directory` carries right now.
/// Reads and parses the file again on each call: the answer is the one the
/// file holds at this moment. A `config_directory` of `None` gives the
/// built-in defaults.
#[must_use]
pub fn load_current_server_config(config_directory: Option<&Path>) -> ServerConfig {
    merge_server(
        ServerConfig::default(),
        load_app_layer(config_directory).into_iter().collect(),
    )
}

/// Whether `koshi.kdl` in `config_directory` carries `allow-other-users` right
/// now.
fn is_other_user_access_allowed(config_directory: Option<&Path>) -> bool {
    load_current_server_config(config_directory).should_allow_other_users
}

/// The machine-wide directory holding the sessions other local users started,
/// or `None` while `allow-other-users` is off in `koshi.kdl` in
/// `config_directory`, while `config_directory` is `None`, or while the machine
/// reports no such directory.
///
/// `koshi.kdl` is read again on each call, so the answer is the one the file
/// holds at this moment.
#[must_use]
pub fn find_shared_sessions_base_directory(config_directory: Option<&Path>) -> Option<PathBuf> {
    let server_config = load_current_server_config(config_directory);
    if !server_config.should_allow_other_users {
        return None;
    }
    resolve_shared_sessions_directory(&server_config)
}

/// Records `koshi.kdl`'s top-level `allow-beta-features` on the beta gate that
/// every `#[beta_feature]` entry point reads.
pub fn apply_beta_gate(app_config_layer: Option<PartialKoshiConfig>) {
    let server_config = merge_server(
        ServerConfig::default(),
        app_config_layer.into_iter().collect(),
    );
    koshi_beta::set_beta_features_allowed(server_config.should_allow_beta_features);
}

/// The split direction a pane-opening verb uses when `--direction` is absent:
/// `app_config_layer`'s `layout.new-pane-direction` folded onto the built-in
/// defaults. The CLI folds the viewer-owned sections exactly as a viewer does.
/// `None` — no config directory, no `koshi.kdl`, or a file that did not parse —
/// gives the built-in [`Direction::Right`].
#[must_use]
pub fn resolve_new_pane_direction(app_config_layer: Option<PartialKoshiConfig>) -> Direction {
    merge_client(
        ClientConfig::default(),
        app_config_layer.into_iter().collect(),
    )
    .layout
    .new_pane_direction
}

/// Whether this viewer sends native image output to its terminal:
/// `app_config_layer`'s `image-support` folded onto the built-in default.
/// `None` gives `true`.
#[must_use]
pub fn supports_image_output(app_config_layer: Option<PartialKoshiConfig>) -> bool {
    merge_client(
        ClientConfig::default(),
        app_config_layer.into_iter().collect(),
    )
    .supports_image_protocols
}

/// The file's text, or `None` when it is absent or unreadable. A read failure
/// other than an absent file, such as a permission error, is recorded in
/// `config_warnings`.
fn load_config_file(config_path: &Path, config_warnings: &mut Vec<String>) -> Option<String> {
    match fs::read_to_string(config_path) {
        Ok(config_source_text) => Some(config_source_text),
        Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => None,
        Err(read_error) => {
            config_warnings.push(format!(
                "could not read config file {}: {read_error}",
                config_path.display()
            ));
            None
        }
    }
}

/// Parses `koshi.kdl` into its override layer and the theme it names,
/// recording every field-partial skip and dropping the file to defaults on a
/// hard error (bad syntax, unknown version, bad `update`).
fn load_app_config(config_path: &Path, config_warnings: &mut Vec<String>) -> Option<AppConfigFile> {
    let config_source_text = load_config_file(config_path, config_warnings)?;
    match parse_app_config(config_path, &config_source_text) {
        Ok(app_config_file) => {
            append_config_field_warnings(
                config_path,
                &app_config_file.parse_warnings,
                config_warnings,
            );
            Some(app_config_file)
        }
        Err(parse_error) => {
            config_warnings.push(format!(
                "koshi.kdl not applied ({}): {parse_error}; using defaults",
                config_path.display()
            ));
            None
        }
    }
}

/// Parses the theme `theme_name` selects — `themes/<theme_name>.kdl` under
/// `config_directory` — into its color layer, naming the layer after the file
/// it came from and recording every field-partial skip.
///
/// Returns `None`, which leaves koshi's built-in colors in place, when
/// `theme_name` is [`DEFAULT_THEME_NAME`], is not a plain file name, or names a
/// file that is absent, unreadable, or fails to parse. Every one of those but
/// the first is recorded in `config_warnings`.
fn load_theme_config(
    config_directory: &Path,
    theme_name: &str,
    config_warnings: &mut Vec<String>,
) -> Option<PartialThemeConfig> {
    if theme_name == DEFAULT_THEME_NAME {
        return None;
    }
    // A theme name is a single file stem under `themes/`, held to the same
    // rule as a profile name: `theme "../../secret"` stops here, before any
    // file is opened.
    if !is_plain_file_name(theme_name) {
        return resolve_default_theme_fallback(
            config_warnings,
            format!("theme name `{theme_name}` must be a plain name"),
        );
    }
    let theme_path = config_directory
        .join("themes")
        .join(format!("{theme_name}.kdl"));
    // One read: an absent file and an unreadable one give different warnings.
    let theme_source_text = match fs::read_to_string(&theme_path) {
        Ok(theme_source_text) => theme_source_text,
        Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => {
            return resolve_default_theme_fallback(
                config_warnings,
                format!("theme `{theme_name}` not found at {}", theme_path.display()),
            );
        }
        Err(read_error) => {
            return resolve_default_theme_fallback(
                config_warnings,
                format!(
                    "theme `{theme_name}` could not be read ({}): {read_error}",
                    theme_path.display()
                ),
            );
        }
    };
    match parse_theme(&theme_path, &theme_source_text) {
        Ok((mut theme_config_layer, field_warnings)) => {
            append_config_field_warnings(&theme_path, &field_warnings, config_warnings);
            // The file carries no name of its own. The layer takes the `name`
            // that selected it.
            theme_config_layer.theme_name = Some(theme_name.to_string());
            Some(theme_config_layer)
        }
        Err(parse_error) => resolve_default_theme_fallback(
            config_warnings,
            format!(
                "theme `{theme_name}` not applied ({}): {parse_error}",
                theme_path.display()
            ),
        ),
    }
}

/// Records `fallback_reason` as the warning for a theme that could not be
/// used, saying which theme stands instead, and yields the `None` that leaves
/// the built-in colors in place.
///
/// Example — `theme "../../x"` gives "theme name `../../x` must be a plain
/// name; using the default theme".
fn resolve_default_theme_fallback(
    config_warnings: &mut Vec<String>,
    fallback_reason: String,
) -> Option<PartialThemeConfig> {
    config_warnings.push(format!(
        "{fallback_reason}; using the {DEFAULT_THEME_NAME} theme"
    ));
    None
}

/// Parses `keybinding.kdl` all-or-nothing: any parse error drops the whole
/// file.
fn load_keybindings_config(
    config_path: &Path,
    config_warnings: &mut Vec<String>,
) -> Option<PartialKeybindingsConfig> {
    let config_source_text = load_config_file(config_path, config_warnings)?;
    match parse_keybindings(config_path, &config_source_text) {
        Ok(keybindings_config_layer) => Some(keybindings_config_layer),
        Err(parse_error) => {
            config_warnings.push(format!(
                "keybinding.kdl not applied ({}): {parse_error}; using defaults",
                config_path.display()
            ));
            None
        }
    }
}

/// Appends each field-partial skip from a parsed file to `config_warnings`,
/// prefixed with the file it came from.
fn append_config_field_warnings(
    config_path: &Path,
    field_warnings: &[String],
    config_warnings: &mut Vec<String>,
) {
    for field_warning in field_warnings {
        config_warnings.push(format!("{}: {field_warning}", config_path.display()));
    }
}

/// Read and parse `profile/<profile_name>.kdl` from `config_directory`. A
/// `config_directory` of `None` returns `None`. A missing, unreadable, or
/// invalid profile is logged and returns `None`; the caller then starts a
/// single shell. Profiles are all-or-nothing: any schema violation drops the
/// whole file, and no pane of a broken profile starts.
#[must_use]
pub fn load_profile_template(
    config_directory: Option<&Path>,
    profile_name: &str,
) -> Option<ProfileTemplate> {
    let config_directory = config_directory?;
    // A profile name is a single file stem under `profile/`. An absolute path,
    // a `..`, or an embedded separator is refused: `--profile ../secret` and
    // `--profile /etc/x` both stop here, before any file is opened.
    if !is_plain_file_name(profile_name) {
        tracing::warn!(
            "profile name `{profile_name}` must be a plain name; starting a single shell"
        );
        return None;
    }
    let profile_path = config_directory
        .join("profile")
        .join(format!("{profile_name}.kdl"));
    let config_source_text = match fs::read_to_string(&profile_path) {
        Ok(config_source_text) => config_source_text,
        Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(path = %profile_path.display(), "profile `{profile_name}` not found; starting a single shell");
            return None;
        }
        Err(read_error) => {
            tracing::warn!(path = %profile_path.display(), %read_error, "profile `{profile_name}` could not be read; starting a single shell");
            return None;
        }
    };
    match parse_profile(&profile_path, &config_source_text) {
        Ok(profile_template) => Some(profile_template),
        Err(profile_error) => {
            tracing::warn!(path = %profile_path.display(), %profile_error, "profile `{profile_name}` not applied; starting a single shell");
            None
        }
    }
}

/// Whether `file_name` is exactly its own file name — no separators, no root
/// or prefix, no `.`/`..`, not empty. A plain name joins to a
/// `<directory>/<file_name>.kdl` directly under `<directory>`, never a nested
/// path and never one that escapes `<directory>`. A file name whose final
/// component differs from the whole string (`../x`, `a/b`, `/etc/x`, `foo/`)
/// is not a plain name.
///
/// Both name-selected config files are held to this: the `--profile <name>`
/// under `profile/` and the `theme "<name>"` under `themes/`.
fn is_plain_file_name(file_name: &str) -> bool {
    Path::new(file_name)
        .file_name()
        .and_then(|file_name_component| file_name_component.to_str())
        == Some(file_name)
}
