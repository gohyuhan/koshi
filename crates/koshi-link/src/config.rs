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
//! file to defaults. [`load_keybinding_file`] reads and parses that file alone
//! and names why a file gives no layer. A conflict in a `keybinding.kdl` that
//! *parses* is caught where the viewer applies it, not here.
//!
//! `load_config_files` writes no log line of its own. It runs before the
//! tracing subscriber is installed, and returns each skip reason as a string
//! the caller replays once tracing is up.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Arc, Mutex, PoisonError};

use koshi_config::app_config::{parse_app_config, AppConfigFile};
use koshi_config::keybinding::{parse_keybindings, KeybindingParseError};
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
#[cfg(unix)]
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
/// started closes the connections it had admitted. The file is parsed again
/// only when its text differs from the text read last.
///
/// The socket's directory is `koshi.kdl`'s `shared-sessions-dir` when it names
/// one, and `/tmp/koshi` otherwise.
#[cfg(unix)]
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
    let shared_sessions_directory = resolve_shared_sessions_directory(&server_config);
    let other_user_access_check: OtherUsersSetting = if is_other_user_access_forced {
        Arc::new(|| true)
    } else {
        let config_path =
            config_directory.map(|config_directory| config_directory.join("koshi.kdl"));
        let last_access_answer = Mutex::new(None);
        Arc::new(move || {
            config_path.as_deref().is_some_and(|config_path| {
                is_other_user_access_allowed(config_path, &last_access_answer)
            })
        })
    };
    Some(OtherUsers {
        shared_directory: shared_sessions_directory,
        is_enabled: other_user_access_check,
    })
}

/// The machine-wide directory the sessions of one user are advertised in:
/// `server_config`'s `shared-sessions-dir` when it names one, and `/tmp/koshi`
/// otherwise.
///
/// The session server creates its socket here, and a `koshi` command looks
/// here for the sessions the other local users started.
#[cfg(unix)]
#[must_use]
pub fn resolve_shared_sessions_directory(server_config: &ServerConfig) -> PathBuf {
    server_config
        .shared_sessions_directory
        .clone()
        .unwrap_or_else(koshi_paths::resolve_shared_sessions_directory)
}

/// The `server` settings `koshi.kdl` in `config_directory` carries right now.
/// Reads and parses the file again on each call: the answer is the one the
/// file holds at this moment. A `config_directory` of `None` gives the
/// built-in defaults.
#[must_use]
pub fn load_current_server_config(config_directory: Option<&Path>) -> ServerConfig {
    match config_directory {
        Some(config_directory) => load_server_config_file(&config_directory.join("koshi.kdl")),
        None => ServerConfig::default(),
    }
}

/// The `server` settings the `koshi.kdl` at `app_config_path` carries right
/// now, folded onto the built-in defaults. Reads and parses the file on each
/// call. A file that is absent, unreadable, or unparseable gives the built-in
/// defaults. Warnings are dropped.
///
/// Example: a file holding `remote-listen "127.0.0.1:7654"` gives a config
/// whose `remote_listen_address` is `127.0.0.1:7654`.
#[must_use]
pub fn load_server_config_file(app_config_path: &Path) -> ServerConfig {
    let mut config_warnings = Vec::new();
    merge_server(
        ServerConfig::default(),
        load_app_config(app_config_path, &mut config_warnings)
            .map(|app_config_file| app_config_file.layer)
            .into_iter()
            .collect(),
    )
}

/// The `allow-other-users` answer for one text of `koshi.kdl`.
#[cfg(unix)]
struct OtherUserAccessAnswer {
    /// The file's text, or `None` when the file was absent or unreadable.
    config_source_text: Option<String>,
    /// Whether that text turns `allow-other-users` on.
    is_allowed: bool,
}

/// Whether `koshi.kdl` at `config_path` carries `allow-other-users` right now.
///
/// The file is read on each call. It is parsed only when its text differs
/// from the text `last_access_answer` holds, which then holds the new text and
/// its answer. A file that is absent, unreadable or not valid gives `false`.
#[cfg(unix)]
fn is_other_user_access_allowed(
    config_path: &Path,
    last_access_answer: &Mutex<Option<OtherUserAccessAnswer>>,
) -> bool {
    let config_source_text = load_config_file(config_path, &mut Vec::new());
    let mut last_access_answer = last_access_answer
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(access_answer) = last_access_answer.as_ref() {
        if access_answer.config_source_text == config_source_text {
            return access_answer.is_allowed;
        }
    }
    let app_config_layer = config_source_text
        .as_deref()
        .and_then(|config_source_text| {
            parse_app_config(config_path, config_source_text)
                .ok()
                .map(|app_config_file| app_config_file.layer)
        });
    let is_allowed = merge_server(
        ServerConfig::default(),
        app_config_layer.into_iter().collect(),
    )
    .should_allow_other_users;
    *last_access_answer = Some(OtherUserAccessAnswer {
        config_source_text,
        is_allowed,
    });
    is_allowed
}

/// The machine-wide directory holding the sessions other local users started,
/// as [`compute_shared_sessions_base_directory`] gives it for `koshi.kdl` in
/// `config_directory`. A `config_directory` of `None` gives `None`.
///
/// `koshi.kdl` is read again on each call, so the answer is the one the file
/// holds at this moment.
#[must_use]
pub fn find_shared_sessions_base_directory(config_directory: Option<&Path>) -> Option<PathBuf> {
    compute_shared_sessions_base_directory(&load_current_server_config(config_directory))
}

/// The machine-wide directory holding the sessions other local users started,
/// as `resolve_shared_sessions_directory` names it, while `server_config`
/// has `allow-other-users` on. `None` while it is off, and always `None` on
/// Windows, where `allow-other-users` and `shared-sessions-dir` have no effect.
///
/// Example: `allow-other-users #true` with no `shared-sessions-dir` gives
/// `Some("/tmp/koshi")` on Linux and macOS, and `None` on Windows.
#[must_use]
pub fn compute_shared_sessions_base_directory(server_config: &ServerConfig) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        server_config
            .should_allow_other_users
            .then(|| resolve_shared_sessions_directory(server_config))
    }
    #[cfg(windows)]
    {
        let _ = server_config;
        None
    }
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

/// This viewer's settings: `app_config_layer` folded onto the built-in
/// defaults. `None` gives [`ClientConfig::default`]. Example: a layer that sets
/// `image-support #false` gives `supports_image_protocols` `false`, with every
/// other field at its default.
#[must_use]
pub fn resolve_client_config(app_config_layer: Option<PartialKoshiConfig>) -> ClientConfig {
    merge_client(
        ClientConfig::default(),
        app_config_layer.into_iter().collect(),
    )
}

/// The file's text, or `None` when it is absent or unreadable. A read failure
/// other than an absent file, such as a permission error, is recorded in
/// `config_warnings`.
fn load_config_file(config_path: &Path, config_warnings: &mut Vec<String>) -> Option<String> {
    read_config_file(config_path).unwrap_or_else(|read_error| {
        config_warnings.push(format_config_read_warning(config_path, &read_error));
        None
    })
}

/// The file's text, or `Ok(None)` when the file is absent.
///
/// # Errors
/// The read error of a file that exists and cannot be read, such as for a
/// permission error or for bytes that are not UTF-8.
fn read_config_file(config_path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(config_path) {
        Ok(config_source_text) => Ok(Some(config_source_text)),
        Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(read_error) => Err(read_error),
    }
}

/// The warning for a config file that exists and cannot be read:
/// `could not read config file <path>: <read error>`.
fn format_config_read_warning(config_path: &Path, read_error: &io::Error) -> String {
    format!(
        "could not read config file {}: {read_error}",
        config_path.display()
    )
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

/// Why a `keybinding.kdl` that exists gives no keybinding layer.
#[derive(Debug)]
pub enum KeybindingFileError {
    /// The file exists and cannot be read, such as for a permission error or
    /// for bytes that are not UTF-8.
    Unreadable(io::Error),
    /// The file does not parse. Any parse error refuses the whole file.
    Unparseable(KeybindingParseError),
}

/// Read and parse the `keybinding.kdl` at `keybinding_file_path`
/// all-or-nothing. Returns `Ok(None)` when the file is absent.
///
/// # Errors
/// [`KeybindingFileError::Unreadable`] when the file exists and cannot be
/// read; [`KeybindingFileError::Unparseable`] when it does not parse.
pub fn load_keybinding_file(
    keybinding_file_path: &Path,
) -> Result<Option<PartialKeybindingsConfig>, KeybindingFileError> {
    let keybinding_source_text = match read_config_file(keybinding_file_path) {
        Ok(Some(keybinding_source_text)) => keybinding_source_text,
        Ok(None) => return Ok(None),
        Err(read_error) => return Err(KeybindingFileError::Unreadable(read_error)),
    };
    parse_keybindings(keybinding_file_path, &keybinding_source_text)
        .map(Some)
        .map_err(KeybindingFileError::Unparseable)
}

/// The [`load_keybinding_file`] layer, or `None` when the file is absent or
/// gives no layer. A file that gives no layer records one warning in
/// `config_warnings`. The warning for a file that does not parse names every
/// problem with its line and column, joined by `; `, such as
/// ``keybinding.kdl not applied (<path>): line 2, column 1: unknown key
/// `unlock_alternative`; did you mean `unlock-alternative`?; using defaults``.
fn load_keybindings_config(
    config_path: &Path,
    config_warnings: &mut Vec<String>,
) -> Option<PartialKeybindingsConfig> {
    match load_keybinding_file(config_path) {
        Ok(keybindings_config_layer) => keybindings_config_layer,
        Err(KeybindingFileError::Unreadable(read_error)) => {
            config_warnings.push(format_config_read_warning(config_path, &read_error));
            None
        }
        Err(KeybindingFileError::Unparseable(parse_error)) => {
            config_warnings.push(format!(
                "keybinding.kdl not applied ({}): {}; using defaults",
                config_path.display(),
                parse_error.list_problem_messages().join("; ")
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
/// single shell. The log line of an invalid profile names every problem with
/// its line and column, joined by `; `. Profiles are all-or-nothing: any schema violation drops the
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
            let profile_problems = profile_error.list_problem_messages().join("; ");
            tracing::warn!(path = %profile_path.display(), %profile_problems, "profile `{profile_name}` not applied; starting a single shell");
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
