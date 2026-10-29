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
//! [`load_app_config_file`] and [`load_config_files_on_app_config`] write no
//! log line of their own. They run before the tracing subscriber is installed,
//! and return each skip reason as a string the caller replays once tracing is
//! up.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Arc, Mutex, PoisonError};

#[cfg(unix)]
use koshi_config::app_config::parse_app_config;
use koshi_config::app_config::AppConfigFile;
use koshi_config::error::ConfigError;
use koshi_config::keybinding::{parse_keybindings, KeybindingParseError};
use koshi_config::layer::{
    merge_client, merge_server, PartialKeybindingsConfig, PartialKoshiConfig, PartialThemeConfig,
};
use koshi_config::parser::ParsedConfigFile;
use koshi_config::profile::{parse_profile, ProfileError};
use koshi_config::theme::parse_theme;
use koshi_config::types::{ClientConfig, ServerConfig, DEFAULT_THEME_NAME};
use koshi_layout::template::ProfileTemplate;
use koshi_observability::logging::{LogFileOwner, LoggingParameters};
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

/// `koshi.kdl` as one read of it left it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LoadedAppConfig {
    /// What the read found.
    pub app_config_outcome: AppConfigOutcome,
    /// One warning per skip, in file order, such as ``<path>: line 2, column
    /// 1: ignored unknown key `frobnicate`; did you mean `update`?``.
    pub config_warnings: Vec<String>,
}

/// What one read of `koshi.kdl` found.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum AppConfigOutcome {
    /// No file was read: no config directory is known, or the file is absent.
    #[default]
    Absent,
    /// The file exists and cannot be read, such as for a permission error or
    /// for bytes that are not UTF-8.
    Unreadable {
        /// The read error, such as `Permission denied (os error 13)`.
        read_error_text: String,
    },
    /// The text was read and does not parse.
    Refused {
        /// Why [`ParsedConfigFile::parse_app_config`] refused the text: the
        /// KDL error, or every problem of the file in file order.
        config_error: ConfigError,
    },
    /// The text parsed.
    Parsed {
        /// What [`ParsedConfigFile::parse_app_config`] gave: the schema
        /// version, the settings the file overrides, the theme name, and the
        /// warnings, each without the file path.
        app_config_file: Box<AppConfigFile>,
    },
}

impl LoadedAppConfig {
    /// The layer of a `koshi.kdl` that parsed. `None` for one that was not
    /// read or does not parse.
    #[must_use]
    pub fn find_app_config_layer(&self) -> Option<&PartialKoshiConfig> {
        match &self.app_config_outcome {
            AppConfigOutcome::Parsed { app_config_file } => Some(&app_config_file.layer),
            AppConfigOutcome::Absent
            | AppConfigOutcome::Unreadable { .. }
            | AppConfigOutcome::Refused { .. } => None,
        }
    }

    /// [`find_app_config_layer`](Self::find_app_config_layer), taking the
    /// layer out.
    #[must_use]
    pub fn into_app_config_layer(self) -> Option<PartialKoshiConfig> {
        match self.app_config_outcome {
            AppConfigOutcome::Parsed { app_config_file } => Some(app_config_file.layer),
            AppConfigOutcome::Absent
            | AppConfigOutcome::Unreadable { .. }
            | AppConfigOutcome::Refused { .. } => None,
        }
    }

    /// [`into_app_config_layer`](Self::into_app_config_layer), and the
    /// warnings the read produced, in file order.
    #[must_use]
    pub fn into_app_config_layer_and_warnings(
        mut self,
    ) -> (Option<PartialKoshiConfig>, Vec<String>) {
        let config_warnings = std::mem::take(&mut self.config_warnings);
        (self.into_app_config_layer(), config_warnings)
    }
}

/// Read and parse `koshi.kdl` in `config_directory`. A file that is absent
/// gives [`AppConfigOutcome::Absent`] and no warning. A file that cannot be
/// read gives [`AppConfigOutcome::Unreadable`] and the warning
/// `could not read config file <path>: <read error>`. Text that is read is
/// parsed once and gives what [`build_loaded_app_config`] states. A
/// `config_directory` of `None`
/// reads nothing and gives [`AppConfigOutcome::Absent`] and the one warning
/// `"no config directory found; using built-in defaults"`.
#[must_use]
pub fn load_app_config_file(config_directory: Option<&Path>) -> LoadedAppConfig {
    load_app_config_file_for_migration(config_directory).0
}

/// [`load_app_config_file`], and the [`ParsedConfigFile`] the
/// [`LoadedAppConfig`] was built from: the text read with its KDL document.
/// The [`ParsedConfigFile`] is `None` when no text was read: no
/// `config_directory`, and a file that is absent or cannot be read.
#[must_use]
pub fn load_app_config_file_for_migration(
    config_directory: Option<&Path>,
) -> (LoadedAppConfig, Option<ParsedConfigFile>) {
    match config_directory {
        Some(config_directory) => load_app_config_at(&config_directory.join("koshi.kdl")),
        None => (
            LoadedAppConfig {
                app_config_outcome: AppConfigOutcome::Absent,
                config_warnings: vec![
                    "no config directory found; using built-in defaults".to_string()
                ],
            },
            None,
        ),
    }
}

/// Read and parse the `koshi.kdl` at `app_config_path`, as
/// [`load_app_config_file_for_migration`] states for a file it finds.
fn load_app_config_at(app_config_path: &Path) -> (LoadedAppConfig, Option<ParsedConfigFile>) {
    match read_config_file(app_config_path) {
        Ok(Some(app_config_source_text)) => {
            let app_config_file = ParsedConfigFile::parse(app_config_path, app_config_source_text);
            (
                build_loaded_app_config(&app_config_file),
                Some(app_config_file),
            )
        }
        Ok(None) => (LoadedAppConfig::default(), None),
        Err(read_error) => (
            LoadedAppConfig {
                app_config_outcome: AppConfigOutcome::Unreadable {
                    read_error_text: read_error.to_string(),
                },
                config_warnings: vec![format_config_read_warning(app_config_path, &read_error)],
            },
            None,
        ),
    }
}

/// The [`LoadedAppConfig`] of `app_config_file`, a `koshi.kdl` read and
/// parsed once. Reads the KDL document `app_config_file` holds and runs no
/// KDL parse.
///
/// A file that parses gives [`AppConfigOutcome::Parsed`] and one warning per
/// field-partial skip, prefixed with the file's path. A file that does not
/// parse (bad syntax, unknown version, bad `update`) gives
/// [`AppConfigOutcome::Refused`] and the one warning
/// `koshi.kdl not applied (<path>): <problems>; using defaults`, where
/// `<problems>` is every problem of the file in file order, joined by `; `.
/// Both outcomes keep what [`ParsedConfigFile::parse_app_config`] gives.
#[must_use]
pub fn build_loaded_app_config(app_config_file: &ParsedConfigFile) -> LoadedAppConfig {
    let app_config_path = app_config_file.get_config_path();
    match app_config_file.parse_app_config() {
        Ok(app_config_file) => {
            let mut config_warnings = Vec::new();
            append_config_field_warnings(
                app_config_path,
                &app_config_file.parse_warnings,
                &mut config_warnings,
            );
            LoadedAppConfig {
                app_config_outcome: AppConfigOutcome::Parsed {
                    app_config_file: Box::new(app_config_file),
                },
                config_warnings,
            }
        }
        Err(config_error) => {
            let config_warnings = vec![format!(
                "koshi.kdl not applied ({}): {config_error}; using defaults",
                app_config_path.display()
            )];
            LoadedAppConfig {
                app_config_outcome: AppConfigOutcome::Refused { config_error },
                config_warnings,
            }
        }
    }
}

/// The config layers in `config_directory`, built on `loaded_app_config`, the
/// `koshi.kdl` already read from it. Reads the theme `loaded_app_config` names
/// and `keybinding.kdl`. Missing, unreadable, or unparseable files are skipped
/// and leave the defaults in place.
///
/// Returns the layers together with a warning per skip, in file order:
/// `loaded_app_config`'s, then the theme's, then `keybinding.kdl`'s. The
/// caller replays the warnings through the log once the tracing subscriber is
/// up. A `config_directory` of `None` reads no file.
#[must_use]
pub fn load_config_files_on_app_config(
    config_directory: Option<&Path>,
    loaded_app_config: LoadedAppConfig,
) -> (LoadedConfig, Vec<String>) {
    let LoadedAppConfig {
        app_config_outcome,
        mut config_warnings,
    } = loaded_app_config;
    let (app_config_layer, theme_name) = match app_config_outcome {
        AppConfigOutcome::Parsed { app_config_file } => {
            (Some(app_config_file.layer), app_config_file.theme_name)
        }
        AppConfigOutcome::Absent
        | AppConfigOutcome::Unreadable { .. }
        | AppConfigOutcome::Refused { .. } => (None, None),
    };
    let Some(config_directory) = config_directory else {
        let loaded_config = LoadedConfig {
            app_config_layer,
            ..LoadedConfig::default()
        };
        return (loaded_config, config_warnings);
    };
    let loaded_config = LoadedConfig {
        app_config_layer,
        theme_config_layer: theme_name.and_then(|theme_name| {
            load_theme_config(config_directory, &theme_name, &mut config_warnings)
        }),
        keybindings_config_layer: load_keybindings_config(
            &config_directory.join("keybinding.kdl"),
            &mut config_warnings,
        ),
    };
    (loaded_config, config_warnings)
}

/// The tracing subscriber's settings for the process `log_file_owner` names:
/// `app_config_layer`'s `logging` section over the built-in defaults. The
/// router, the session server, every client attached to a session, and on
/// Windows the process holding a session's panes build their parameters here.
/// One session's lines all land in one file, and the router's in another.
/// `max-log-file-size-in-mb 4` gives a `max_log_file_size_bytes` of 4194304.
#[must_use]
pub fn build_logging_parameters(
    app_config_layer: Option<&PartialKoshiConfig>,
    log_file_owner: LogFileOwner,
) -> LoggingParameters {
    let logging_config = app_config_layer
        .map(PartialKoshiConfig::get_logging_config)
        .unwrap_or_default();
    LoggingParameters {
        is_enabled: logging_config.is_enabled,
        log_level: logging_config.log_level,
        log_format: logging_config.log_format,
        log_file_owner,
        max_log_file_size_bytes: u64::from(logging_config.max_log_file_size_in_mb) * 1024 * 1024,
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
    let server_config = resolve_server_config(app_config_layer);
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

/// The `server` settings the `koshi.kdl` at `app_config_path` carries right
/// now, folded onto the built-in defaults. Reads and parses the file on each
/// call. A file that is absent, unreadable, or unparseable gives the built-in
/// defaults. Warnings are dropped.
///
/// Example: a file holding `remote-listen "127.0.0.1:7654"` gives a config
/// whose `remote_listen_address` is `127.0.0.1:7654`.
#[must_use]
pub fn load_server_config_file(app_config_path: &Path) -> ServerConfig {
    merge_server(
        ServerConfig::default(),
        load_app_config_at(app_config_path)
            .0
            .into_app_config_layer()
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
    let config_source_text = read_config_file(config_path).unwrap_or(None);
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

/// The server settings `app_config_layer` gives, folded onto the built-in
/// defaults. `None` gives [`ServerConfig::default`]. Example: a layer that sets
/// `remote-listen "127.0.0.1:7654"` gives `remote_listen_address`
/// `127.0.0.1:7654`, with every other field at its default.
#[must_use]
pub fn resolve_server_config(app_config_layer: Option<&PartialKoshiConfig>) -> ServerConfig {
    merge_server(
        ServerConfig::default(),
        app_config_layer.cloned().into_iter().collect(),
    )
}

/// The machine-wide directory holding the sessions other local users started,
/// as [`compute_shared_sessions_base_directory`] gives it for the `koshi.kdl`
/// layer `app_config_layer` folded onto the built-in defaults. A layer of
/// `None` gives `None`.
#[must_use]
pub fn resolve_shared_sessions_base_directory_for_layer(
    app_config_layer: Option<&PartialKoshiConfig>,
) -> Option<PathBuf> {
    compute_shared_sessions_base_directory(&resolve_server_config(app_config_layer))
}

/// The machine-wide directory holding the sessions other local users started,
/// as [`compute_shared_sessions_base_directory`] gives it for `koshi.kdl` in
/// `config_directory`. A `config_directory` of `None` gives `None`.
///
/// `koshi.kdl` is read again on each call, so the answer is the one the file
/// holds at this moment.
#[must_use]
pub fn find_shared_sessions_base_directory(config_directory: Option<&Path>) -> Option<PathBuf> {
    let config_directory = config_directory?;
    compute_shared_sessions_base_directory(&load_server_config_file(
        &config_directory.join("koshi.kdl"),
    ))
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

/// This viewer's settings: `app_config_layer` folded onto the built-in
/// defaults. `None` gives [`ClientConfig::default`]. Example: a layer that sets
/// `image-support #false` gives `supports_image_protocols` `false`, with every
/// other field at its default. The CLI folds the same sections: a
/// pane-opening verb given no `--direction` splits toward
/// `layout.new_pane_direction`, which is
/// [`Direction::Right`](koshi_core::geometry::Direction::Right) with no
/// `koshi.kdl`.
#[must_use]
pub fn resolve_client_config(app_config_layer: Option<PartialKoshiConfig>) -> ClientConfig {
    merge_client(
        ClientConfig::default(),
        app_config_layer.into_iter().collect(),
    )
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

/// Parses the theme `theme_name` selects — `themes/<theme_name>.kdl` under
/// `config_directory` — into its color layer, naming the layer after the file
/// it came from and recording every field-partial skip.
///
/// Returns `None`, which leaves koshi's built-in colors in place, when
/// `theme_name` is [`DEFAULT_THEME_NAME`], is not a plain file name, or names a
/// file that is absent, unreadable, or fails to parse. Every one of those but
/// the first is recorded in `config_warnings`. A file that fails to parse
/// gives one warning that names every problem of the file in file order,
/// joined by `; `.
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
/// prefixed with the file it came from, such as ``<path>: line 2, column 1:
/// ignored unknown key `frobnicate`; did you mean `update`?``.
fn append_config_field_warnings(
    config_path: &Path,
    field_warnings: &[String],
    config_warnings: &mut Vec<String>,
) {
    for field_warning in field_warnings {
        config_warnings.push(format!("{}: {field_warning}", config_path.display()));
    }
}

/// One read of `profile/<profile_name>.kdl`, as [`load_profile_file`] gives
/// it.
#[derive(Debug)]
pub struct LoadedProfile {
    /// The name `--profile` gave, such as `work`.
    pub profile_name: String,
    /// What the read found.
    pub profile_outcome: ProfileOutcome,
}

/// What one read of a profile file found.
#[derive(Debug)]
pub enum ProfileOutcome {
    /// The profile name is not a plain file name, such as `../secret`. No file
    /// was read.
    NotPlainName,
    /// No file is at `profile_path`.
    Absent {
        /// The path read: `<config directory>/profile/<profile_name>.kdl`.
        profile_path: PathBuf,
    },
    /// The file at `profile_path` exists and cannot be read, such as for a
    /// permission error or for bytes that are not UTF-8.
    Unreadable {
        /// The path read.
        profile_path: PathBuf,
        /// The read error.
        read_error: io::Error,
    },
    /// The text was read and does not parse. Profiles are all-or-nothing: any
    /// schema violation refuses the whole file.
    Refused {
        /// The path read.
        profile_path: PathBuf,
        /// Why [`parse_profile`] refused the text.
        profile_error: ProfileError,
    },
    /// The text parsed.
    Parsed {
        /// The tabs and panes the profile opens.
        profile_template: ProfileTemplate,
    },
}

/// Read and parse `profile/<profile_name>.kdl` from `config_directory`.
/// Writes no log line: [`resolve_profile_template`] writes the line for a
/// profile that gives no template.
///
/// A `profile_name` that is not a plain file name gives
/// [`ProfileOutcome::NotPlainName`] and opens no file. When
/// `parsed_config_files` holds the file at the profile's path, the profile is
/// read from that parse, and the file is not read again. Every other profile
/// is read from disk: a file that is absent gives [`ProfileOutcome::Absent`],
/// and one that cannot be read gives [`ProfileOutcome::Unreadable`]. Text
/// that parses gives [`ProfileOutcome::Parsed`], and text that does not gives
/// [`ProfileOutcome::Refused`].
#[must_use]
pub fn load_profile_file(
    config_directory: &Path,
    profile_name: &str,
    parsed_config_files: &[ParsedConfigFile],
) -> LoadedProfile {
    // A profile name is a single file stem under `profile/`. An absolute path,
    // a `..`, or an embedded separator is refused: `--profile ../secret` and
    // `--profile /etc/x` both stop here, before any file is opened.
    let profile_outcome = if is_plain_file_name(profile_name) {
        let profile_path = config_directory
            .join("profile")
            .join(format!("{profile_name}.kdl"));
        let parsed_profile_file = parsed_config_files
            .iter()
            .find(|parsed_config_file| parsed_config_file.get_config_path() == profile_path);
        match parsed_profile_file {
            Some(parsed_profile_file) => {
                build_profile_outcome(profile_path, parsed_profile_file.parse_profile())
            }
            None => load_profile_outcome(profile_path),
        }
    } else {
        ProfileOutcome::NotPlainName
    };
    LoadedProfile {
        profile_name: profile_name.to_string(),
        profile_outcome,
    }
}

/// Read and parse the profile at `profile_path`, as [`load_profile_file`]
/// states for a profile it reads from disk.
fn load_profile_outcome(profile_path: PathBuf) -> ProfileOutcome {
    match fs::read_to_string(&profile_path) {
        Ok(profile_source_text) => {
            let profile_parse_result = parse_profile(&profile_path, &profile_source_text);
            build_profile_outcome(profile_path, profile_parse_result)
        }
        Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => {
            ProfileOutcome::Absent { profile_path }
        }
        Err(read_error) => ProfileOutcome::Unreadable {
            profile_path,
            read_error,
        },
    }
}

/// [`ProfileOutcome::Parsed`] for a template, and [`ProfileOutcome::Refused`]
/// with `profile_path` for an error.
fn build_profile_outcome(
    profile_path: PathBuf,
    profile_parse_result: Result<ProfileTemplate, ProfileError>,
) -> ProfileOutcome {
    match profile_parse_result {
        Ok(profile_template) => ProfileOutcome::Parsed { profile_template },
        Err(profile_error) => ProfileOutcome::Refused {
            profile_path,
            profile_error,
        },
    }
}

/// The template `loaded_profile` gives, or `None`, and the caller then starts
/// a single shell. A profile that gives no template writes one warn line:
///
/// - [`ProfileOutcome::NotPlainName`]: ``profile name `<name>` must be a
///   plain name; starting a single shell``.
/// - [`ProfileOutcome::Absent`]: ``profile `<name>` not found; starting a
///   single shell``, with the `path` field.
/// - [`ProfileOutcome::Unreadable`]: ``profile `<name>` could not be read;
///   starting a single shell``, with the `path` and `read_error` fields.
/// - [`ProfileOutcome::Refused`]: ``profile `<name>` not applied; starting a
///   single shell``, with the `path` field and the `profile_problems` field,
///   which names every problem with its line and column, joined by `; `.
#[must_use]
pub fn resolve_profile_template(loaded_profile: LoadedProfile) -> Option<ProfileTemplate> {
    let LoadedProfile {
        profile_name,
        profile_outcome,
    } = loaded_profile;
    match profile_outcome {
        ProfileOutcome::Parsed { profile_template } => Some(profile_template),
        ProfileOutcome::NotPlainName => {
            tracing::warn!(
                "profile name `{profile_name}` must be a plain name; starting a single shell"
            );
            None
        }
        ProfileOutcome::Absent { profile_path } => {
            tracing::warn!(path = %profile_path.display(), "profile `{profile_name}` not found; starting a single shell");
            None
        }
        ProfileOutcome::Unreadable {
            profile_path,
            read_error,
        } => {
            tracing::warn!(path = %profile_path.display(), %read_error, "profile `{profile_name}` could not be read; starting a single shell");
            None
        }
        ProfileOutcome::Refused {
            profile_path,
            profile_error,
        } => {
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
