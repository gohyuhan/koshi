//! Local `koshi config` command implementation.
//!
//! The command scans only Koshi's known config paths: `koshi.kdl`,
//! `keybinding.kdl`, `themes/*.kdl`, and `profile/*.kdl`. Validation reads
//! every present regular file and reports all read and schema errors together.
//! Migration keeps config symlinks, validates every result in memory, then
//! atomically replaces each changed file or symlink target. A write failure
//! names files already migrated and marks the failing file as possibly changed.
//! A migration parses each text it reads once, and parses a text that a read
//! before the migration lock already parsed no second time.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use koshi_config::migration::{
    migrate_config, validate_config, validate_parsed_app_config, ConfigFileKind, MigratedConfig,
    MigrationError, ValidatedConfig,
};
use koshi_config::parser::{format_unknown_key, ParsedConfigFile};
use koshi_storage::{atomic::write_atomic, error::StorageError};

use crate::cli::{CliCommand, ConfigCommand};
use koshi_link::config::{
    build_loaded_app_config, load_app_config_file, load_app_config_file_for_migration,
    load_profile_file, AppConfigOutcome, LoadedAppConfig, LoadedProfile,
};
use koshi_link::error::CliError;

#[cfg(test)]
mod tests;

struct ConfigFile {
    config_file_kind: ConfigFileKind,
    config_path: PathBuf,
    config_write_path: PathBuf,
    config_source_text: String,
}

struct LoadedConfigFiles {
    config_files: Vec<ConfigFile>,
    config_file_read_errors: Vec<String>,
}

/// One config file a migration read, and what migrating it gave.
struct ConfigFileMigration {
    /// The text read, with its KDL document.
    source_config_file: ParsedConfigFile,
    /// What the migration function gave for `source_config_file`.
    migration_result: Result<MigratedConfig, MigrationError>,
}

impl ConfigFileMigration {
    /// Migrates `source_config_file`, a config file of kind
    /// `config_file_kind`, with `migrate_config_function`.
    fn from_source_config_file(
        config_file_kind: ConfigFileKind,
        source_config_file: ParsedConfigFile,
        migrate_config_function: ConfigMigrationFunction,
    ) -> Self {
        let migration_result = migrate_config_function(config_file_kind, &source_config_file);
        Self {
            source_config_file,
            migration_result,
        }
    }
}

/// What migrating every config file in one directory produced.
struct ConfigMigrationReport {
    /// One line per file, in path order, each ending in `\n`:
    /// `"/home/user/.config/koshi/koshi.kdl: migrated version 1 to 2"` for a
    /// file written on the new schema, and
    /// `"/home/user/.config/koshi/koshi.kdl: current (version 2)"` for one left
    /// as it is. A directory holding no config file gives the one line
    /// `"no config files found in <directory>"`. `Err` holds the failure that
    /// stopped the migration.
    config_report_text: Result<String, CliError>,
    /// Every config file whose text is known once the migration ends, with
    /// its KDL document: the migrated text for a file the migration wrote, and
    /// the text read last for every other file it read. A file that could not
    /// be read and a file whose write failed are missing.
    config_files: Vec<ParsedConfigFile>,
}

/// The config one command starts on, read as [`load_config_for_command`]
/// states.
pub struct StartupConfig {
    /// The `koshi.kdl` the command starts on.
    pub loaded_app_config: LoadedAppConfig,
    /// The failure of the config migration a `serve-session` or
    /// `serve-router` ran. `None` for a migration that did not fail, and for
    /// every other command.
    pub config_migration_error: Option<CliError>,
    /// The profile a `serve-session` that seeds a new session opens. `None`
    /// for a start without `--profile`, for a resume run, with no config
    /// directory, and for every other command.
    pub loaded_profile: Option<LoadedProfile>,
    /// The `koshi.kdl` text `koshi config migrate` read at start, with its KDL
    /// document. `None` when it read no text, and for every other command.
    pub app_config_file_for_migration: Option<ParsedConfigFile>,
}

struct ConfigFieldHelp {
    config_key: &'static str,
    config_file_name: &'static str,
    default_config_value: &'static str,
    config_field_description: &'static str,
}

/// One row for each key `koshi config explain` prints: every key the
/// `koshi.kdl`, `keybinding.kdl`, theme, and profile parsers accept, except the
/// `mode` blocks of `keybinding.kdl` and the `tab` blocks of a profile.
const CONFIG_FIELD_HELP: &[ConfigFieldHelp] = &[
    build_config_field_help("koshi.version", "koshi.kdl", "2", "Config schema version."),
    build_config_field_help(
        "koshi.theme",
        "koshi.kdl",
        "\"default\"",
        "Color theme file name.",
    ),
    build_config_field_help(
        "koshi.allow-beta-features",
        "koshi.kdl",
        "#false",
        "Run features still marked beta.",
    ),
    build_config_field_help(
        "koshi.allow-other-users",
        "koshi.kdl",
        "#false",
        "Let other users of this machine reach your sessions. macOS and Linux only; no effect \
         on Windows.",
    ),
    build_config_field_help(
        "koshi.shared-sessions-dir",
        "koshi.kdl",
        "\"/tmp/koshi\"",
        "Directory the shared session sockets live in. No effect on Windows.",
    ),
    build_config_field_help(
        "koshi.remote-listen",
        "koshi.kdl",
        "unset",
        "IP address and port the remote listener binds to, such as 192.0.2.20:7654. Setting \
         it opens nothing; `koshi share open` opens the port. The router reads it when it \
         starts, so a change applies after `koshi restart-servers`.",
    ),
    build_config_field_help(
        "koshi.auto-close-session",
        "koshi.kdl",
        "#false",
        "End the session when its last client leaves.",
    ),
    build_config_field_help(
        "koshi.pane-clipboard-writes.allow",
        "koshi.kdl",
        "#false",
        "Let a program in a pane write the clipboard of the clients that focus that pane.",
    ),
    build_config_field_help(
        "koshi.pane-clipboard-writes.max-copy-bytes",
        "koshi.kdl",
        "8388608",
        "Largest copy a pane may write, in bytes. A bigger OSC 52 copy makes no copy.",
    ),
    build_config_field_help(
        "koshi.remote-reconnect",
        "koshi.kdl",
        "#true",
        "Dial a session on another machine again when the link drops.",
    ),
    build_config_field_help(
        "koshi.remote-link-timeout-ms",
        "koshi.kdl",
        "30000",
        "Milliseconds a link to a session on another machine may carry no byte before it \
         drops, from 1000.",
    ),
    build_config_field_help(
        "koshi.max-frames-per-second",
        "koshi.kdl",
        "60",
        "Most frames a second the session sends this terminal, from 30 to 120.",
    ),
    build_config_field_help(
        "koshi.image-support",
        "koshi.kdl",
        "#true",
        "Send native image output (Kitty, iTerm2, or Sixel) to this terminal.",
    ),
    build_config_field_help(
        "koshi.reduced-motion",
        "koshi.kdl",
        "#false",
        "Draw placement previews, other viewers' placements and the notification toast without \
         sliding.",
    ),
    build_config_field_help(
        "koshi.stay-in-pane-placement-mode-after-placement",
        "koshi.kdl",
        "#true",
        "Keep pane placement mode opened by a key, and floating pane move mode, on after the \
         session accepts a placement or runs a move. The mode that a drag of a pane's grab \
         handle opens ends with its drop.",
    ),
    build_config_field_help(
        "koshi.pane.min-cols",
        "koshi.kdl",
        "2",
        "Smallest pane width in columns.",
    ),
    build_config_field_help(
        "koshi.pane.min-rows",
        "koshi.kdl",
        "1",
        "Smallest pane height in rows.",
    ),
    build_config_field_help(
        "koshi.pane.gap",
        "koshi.kdl",
        "0",
        "Blank cells between two panes that meet along a split.",
    ),
    build_config_field_help(
        "koshi.scrollback.max-lines",
        "koshi.kdl",
        "10000",
        "Most saved lines per pane.",
    ),
    build_config_field_help(
        "koshi.scrollback.max-bytes",
        "koshi.kdl",
        "33554432",
        "Most saved text bytes per pane.",
    ),
    build_config_field_help(
        "koshi.scrollback.scroll-on-input",
        "koshi.kdl",
        "#true",
        "Jump to newest output after input.",
    ),
    build_config_field_help(
        "koshi.layout.new-pane-direction",
        "koshi.kdl",
        "\"right\"",
        "Default side for a new pane.",
    ),
    build_config_field_help(
        "koshi.mouse.border-resize",
        "koshi.kdl",
        "#true",
        "Allow border dragging to resize panes.",
    ),
    build_config_field_help(
        "koshi.mouse.scroll-lines",
        "koshi.kdl",
        "3",
        "Lines moved by one wheel step.",
    ),
    build_config_field_help(
        "koshi.mouse.wheel",
        "koshi.kdl",
        "\"scroll-scrollback\"",
        "Wheel action over a plain pane.",
    ),
    build_config_field_help(
        "koshi.copy.trim-trailing-whitespace",
        "koshi.kdl",
        "#true",
        "Trim line-end spaces when copying.",
    ),
    build_config_field_help(
        "koshi.notifications.toast",
        "koshi.kdl",
        "#true",
        "Slide each new notification in as a toast. The notification center keeps every \
         notification either way.",
    ),
    build_config_field_help(
        "koshi.notifications.toast-position",
        "koshi.kdl",
        "\"top-right\"",
        "Corner of the pane area the toast sits in: \"top-right\", \"top-left\", \
         \"bottom-right\" or \"bottom-left\".",
    ),
    build_config_field_help(
        "koshi.notifications.toast-duration-ms",
        "koshi.kdl",
        "3000",
        "How long a toast stays in place, in milliseconds, from 1000 to 60000.",
    ),
    build_config_field_help(
        "koshi.terminal.term",
        "koshi.kdl",
        "\"xterm-256color\"",
        "TERM value given to child programs.",
    ),
    build_config_field_help(
        "koshi.terminal.colorterm",
        "koshi.kdl",
        "\"truecolor\"",
        "COLORTERM value given to child programs.",
    ),
    build_config_field_help(
        "koshi.terminal.default-shell",
        "koshi.kdl",
        "$SHELL (%COMSPEC% on Windows)",
        "Shell used for a new terminal pane.",
    ),
    build_config_field_help(
        "koshi.terminal.extended-keys",
        "koshi.kdl",
        "\"on-request\"",
        "What a pane program receives for a key whose legacy bytes another key also sends: \
         \"on-request\" or \"always\".",
    ),
    build_config_field_help(
        "koshi.logging.enabled",
        "koshi.kdl",
        "#false",
        "Write Koshi log files.",
    ),
    build_config_field_help(
        "koshi.logging.level",
        "koshi.kdl",
        "\"warning\"",
        "Lowest log level written.",
    ),
    build_config_field_help(
        "koshi.logging.format",
        "koshi.kdl",
        "\"pretty\"",
        "Human or JSON log format.",
    ),
    build_config_field_help(
        "koshi.logging.max-log-file-size-in-mb",
        "koshi.kdl",
        "4",
        "Size of a log file in MiB, from 1 to 1024. A bigger file moves to <name>.1.",
    ),
    build_config_field_help(
        "koshi.update.auto-check",
        "koshi.kdl",
        "#true",
        "Check for updates on interactive start.",
    ),
    build_config_field_help(
        "koshi.update.check-interval-days",
        "koshi.kdl",
        "14",
        "Days between startup update checks.",
    ),
    build_config_field_help(
        "koshi.update.allow-prerelease",
        "koshi.kdl",
        "#false",
        "Include prerelease builds in update checks.",
    ),
    build_config_field_help(
        "keybinding.version",
        "keybinding.kdl",
        "2",
        "Config schema version.",
    ),
    build_config_field_help(
        "keybinding.chord-timeout-ms",
        "keybinding.kdl",
        "500",
        "Wait for the next key of a shortcut that is complete and also starts a longer one.",
    ),
    build_config_field_help(
        "keybinding.which-key-delay-ms",
        "keybinding.kdl",
        "300",
        "Wait after a shortcut's first key before showing the keys that continue it.",
    ),
    build_config_field_help(
        "keybinding.max-chord-depth",
        "keybinding.kdl",
        "4",
        "Most keys in one sequence.",
    ),
    build_config_field_help(
        "keybinding.leader",
        "keybinding.kdl",
        "\"C-\"",
        "Prefix used by `<leader>` bindings.",
    ),
    build_config_field_help(
        "keybinding.unlock-alternative",
        "keybinding.kdl",
        "unset",
        "Replacement key that always unlocks input.",
    ),
    build_config_field_help(
        "theme.version",
        "themes/<name>.kdl",
        "2",
        "Config schema version.",
    ),
    build_config_field_help(
        "theme.colors.ramp-start",
        "themes/<name>.kdl",
        "\"#d0a5ff\"",
        "First chrome gradient color.",
    ),
    build_config_field_help(
        "theme.colors.ramp-end",
        "themes/<name>.kdl",
        "\"#7dbcff\"",
        "Last chrome gradient color.",
    ),
    build_config_field_help(
        "theme.colors.on-ramp",
        "themes/<name>.kdl",
        "\"#12091f\"",
        "Text over the chrome gradient.",
    ),
    build_config_field_help(
        "theme.colors.on-ramp-dim",
        "themes/<name>.kdl",
        "\"#f0ecfa\"",
        "Dim text over the chrome gradient.",
    ),
    build_config_field_help(
        "theme.colors.accent",
        "themes/<name>.kdl",
        "\"#f5c2ff\"",
        "Color for a key sequence in progress.",
    ),
    build_config_field_help(
        "theme.colors.on-accent",
        "themes/<name>.kdl",
        "\"#1e1033\"",
        "Text over the accent color.",
    ),
    build_config_field_help(
        "theme.colors.bar-bg",
        "themes/<name>.kdl",
        "\"#000000\"",
        "Tab and key-hint bar background.",
    ),
    build_config_field_help(
        "theme.colors.border-focused",
        "themes/<name>.kdl",
        "\"#00afd7\"",
        "Focused pane border.",
    ),
    build_config_field_help(
        "theme.colors.border-unfocused",
        "themes/<name>.kdl",
        "\"#585858\"",
        "Unfocused pane border.",
    ),
    build_config_field_help(
        "theme.colors.border-hover",
        "themes/<name>.kdl",
        "\"#af5fff\"",
        "Pane border under the pointer.",
    ),
    build_config_field_help(
        "theme.colors.stack-header-fg",
        "themes/<name>.kdl",
        "\"#f4f1fa\"",
        "Collapsed stack header text.",
    ),
    build_config_field_help(
        "theme.colors.stack-header-bg",
        "themes/<name>.kdl",
        "\"#300f4a\"",
        "Collapsed stack header background.",
    ),
    build_config_field_help(
        "theme.colors.letterbox",
        "themes/<name>.kdl",
        "\"#585858\"",
        "Margin around a centered layout.",
    ),
    build_config_field_help(
        "profile.version",
        "profile/<name>.kdl",
        "2",
        "Config schema version.",
    ),
    build_config_field_help(
        "profile.lock",
        "profile/<name>.kdl",
        "unset",
        "Bare marker. Start the session's first terminal with input locked.",
    ),
];

const fn build_config_field_help(
    config_key: &'static str,
    config_file_name: &'static str,
    default_config_value: &'static str,
    config_field_description: &'static str,
) -> ConfigFieldHelp {
    ConfigFieldHelp {
        config_key,
        config_file_name,
        default_config_value,
        config_field_description,
    }
}

/// Runs one config command and prints its result. `koshi config check`
/// validates `koshi.kdl` from what the read at start found,
/// `app_config_outcome`, and reads every other config file from disk.
/// `koshi config migrate` reuses `app_config_file_for_migration`, the
/// `koshi.kdl` it read at start, when the text it reads under the migration
/// lock is the same.
///
/// # Errors
/// Returns [`CliError::Config`] when the platform has no config directory or
/// a file cannot be read, validated, explained, or replaced.
pub fn run_config_command(
    command: &ConfigCommand,
    app_config_outcome: &AppConfigOutcome,
    app_config_file_for_migration: Option<ParsedConfigFile>,
) -> Result<(), CliError> {
    let config_directory =
        koshi_paths::resolve_config_directory().ok_or_else(|| CliError::Config {
            detail: "platform config directory is unavailable".to_string(),
        })?;
    let config_command_output = run_config_command_in_directory(
        command,
        &config_directory,
        app_config_outcome,
        app_config_file_for_migration,
    )?;
    print!("{config_command_output}");
    Ok(())
}

/// The config `command` starts on, read from `config_directory`.
///
/// `serve-session` and `serve-router` first migrate the config files. The
/// migration reads every config file once and parses each text once. It
/// keeps the text it read of each file, or the text it wrote for a file it
/// migrated, and keeps no text of a file it could not read or whose write
/// failed. The command starts on the `koshi.kdl` text the migration kept, and
/// a `serve-session` that seeds its session opens the `--profile` text the
/// migration kept. A profile with no kept text is read from disk. A
/// `koshi.kdl` with no kept text gives no layer after a migration that did not
/// fail, and is read from disk after one that failed. A migration that fails
/// gives its error.
///
/// `koshi config migrate` reads `koshi.kdl` as
/// [`load_app_config_file_for_migration`] states and keeps the parse of the
/// text it read. Every other command, `resume-support` among them, reads
/// `koshi.kdl` as [`load_app_config_file`] states. A `config_directory` of
/// `None` migrates nothing and reads no file. A `koshi.kdl` that is absent
/// gives no layer and no warning.
#[must_use]
pub fn load_config_for_command(
    command: Option<&CliCommand>,
    config_directory: Option<&Path>,
) -> StartupConfig {
    match (command, config_directory) {
        (
            Some(CliCommand::ServeSession {
                profile_name,
                resume_state_path,
                ..
            }),
            Some(config_directory),
        ) => {
            let seeded_profile_name = match resume_state_path {
                Some(_) => None,
                None => profile_name.as_deref(),
            };
            let (config_files, config_migration_error) =
                migrate_config_directory_for_update(config_directory);
            build_service_startup_config(
                config_directory,
                seeded_profile_name,
                &config_files,
                config_migration_error,
            )
        }
        (Some(CliCommand::ServeRouter { .. }), Some(config_directory)) => {
            let (config_files, config_migration_error) =
                migrate_config_directory_for_update(config_directory);
            build_service_startup_config(
                config_directory,
                None,
                &config_files,
                config_migration_error,
            )
        }
        (
            Some(CliCommand::Config {
                command: ConfigCommand::Migrate,
            }),
            _,
        ) => {
            let (loaded_app_config, app_config_file_for_migration) =
                load_app_config_file_for_migration(config_directory);
            StartupConfig {
                loaded_app_config,
                config_migration_error: None,
                loaded_profile: None,
                app_config_file_for_migration,
            }
        }
        _ => StartupConfig {
            loaded_app_config: load_app_config_file(config_directory),
            config_migration_error: None,
            loaded_profile: None,
            app_config_file_for_migration: None,
        },
    }
}

/// The [`StartupConfig`] of a `serve-session` or `serve-router` whose config
/// migration in `config_directory` gave `config_files` and
/// `config_migration_error`, as [`migrate_config_directory_for_update`]
/// states. Parses no text again that `config_files` holds.
///
/// `koshi.kdl` comes from its file in `config_files`, as
/// [`build_loaded_app_config`] states. A `koshi.kdl` missing from
/// `config_files` gives [`LoadedAppConfig::default`] when the migration did
/// not fail, and the file [`load_app_config_file`] reads when it failed.
/// `seeded_profile_name`, the `--profile` of a session that seeds itself, is
/// read as [`load_profile_file`] states, from its file in `config_files` when
/// they hold it.
fn build_service_startup_config(
    config_directory: &Path,
    seeded_profile_name: Option<&str>,
    config_files: &[ParsedConfigFile],
    config_migration_error: Option<CliError>,
) -> StartupConfig {
    let app_config_path = config_directory.join("koshi.kdl");
    let app_config_file = config_files
        .iter()
        .find(|config_file| config_file.get_config_path() == app_config_path);
    let loaded_app_config = match (app_config_file, &config_migration_error) {
        (Some(app_config_file), _) => build_loaded_app_config(app_config_file),
        (None, None) => LoadedAppConfig::default(),
        (None, Some(_)) => load_app_config_file(Some(config_directory)),
    };
    let loaded_profile = seeded_profile_name
        .map(|profile_name| load_profile_file(config_directory, profile_name, config_files));
    StartupConfig {
        loaded_app_config,
        config_migration_error,
        loaded_profile,
        app_config_file_for_migration: None,
    }
}

/// Migrate saved KDL files before a replacement server reads config. Gives
/// every config file whose text is known once the migration ends, with its
/// KDL document, and the error of a migration that failed.
///
/// Reads every config file once and parses each text once. A directory whose
/// files all use the current schema takes no lock, writes nothing, and gives
/// every file as read. Otherwise the files migrate under the lock, as
/// [`migrate_config_directory_with_lock`] states, and a file whose text is
/// unchanged under the lock keeps the parse and the migration of this read.
///
/// A file with schema problems, such as an unknown key, is migrated with those
/// problems left in its text.
///
/// The error is a read error for the directory or a file, any
/// [`migrate_config`] error, or a lock or write error. A read error or a
/// [`migrate_config`] error gives every file that was read, as read, and takes
/// no lock.
fn migrate_config_directory_for_update(
    config_directory: &Path,
) -> (Vec<ParsedConfigFile>, Option<CliError>) {
    let loaded_config_files = load_config_files(config_directory);
    let mut migration_errors = loaded_config_files.config_file_read_errors;
    let mut config_file_migrations = Vec::with_capacity(loaded_config_files.config_files.len());
    let mut has_older_schema_file = false;
    for config_file in loaded_config_files.config_files {
        let config_file_migration = ConfigFileMigration::from_source_config_file(
            config_file.config_file_kind,
            ParsedConfigFile::parse(&config_file.config_path, config_file.config_source_text),
            migrate_config,
        );
        match &config_file_migration.migration_result {
            Ok(migrated_config) => {
                if migrated_config.migrated_config_file.is_some() {
                    has_older_schema_file = true;
                }
            }
            Err(migration_error) => migration_errors.push(migration_error.to_string()),
        }
        config_file_migrations.push(config_file_migration);
    }
    if !migration_errors.is_empty() {
        let config_migration_error = CliError::Config {
            detail: migration_errors.join("\n"),
        };
        return (
            list_source_config_files(config_file_migrations),
            Some(config_migration_error),
        );
    }
    if !has_older_schema_file {
        return (list_source_config_files(config_file_migrations), None);
    }
    let config_migration_report =
        migrate_config_directory_with_lock(config_directory, config_file_migrations);
    (
        config_migration_report.config_files,
        config_migration_report.config_report_text.err(),
    )
}

/// The source file of each migration in `config_file_migrations`, in the same
/// order.
fn list_source_config_files(
    config_file_migrations: Vec<ConfigFileMigration>,
) -> Vec<ParsedConfigFile> {
    config_file_migrations
        .into_iter()
        .map(|config_file_migration| config_file_migration.source_config_file)
        .collect()
}

fn run_config_command_in_directory(
    command: &ConfigCommand,
    config_directory: &Path,
    app_config_outcome: &AppConfigOutcome,
    app_config_file_for_migration: Option<ParsedConfigFile>,
) -> Result<String, CliError> {
    match command {
        ConfigCommand::Path => Ok(format!("{}\n", config_directory.display())),
        ConfigCommand::Explain { config_key } => explain_config_key(config_key),
        ConfigCommand::Check => check_config_directory(config_directory, app_config_outcome),
        ConfigCommand::Migrate => {
            let earlier_config_migrations = app_config_file_for_migration
                .into_iter()
                .map(|app_config_file| {
                    ConfigFileMigration::from_source_config_file(
                        ConfigFileKind::App,
                        app_config_file,
                        migrate_config,
                    )
                })
                .collect();
            migrate_config_directory_with_lock(config_directory, earlier_config_migrations)
                .config_report_text
        }
    }
}

/// Migrate every config file in `config_directory` under the migration lock,
/// as [`migrate_config_directory_with`] states, with [`migrate_config`] and
/// [`write_atomic`]. `earlier_config_migrations` are the migrations of files
/// read before the lock was taken.
///
/// A `config_directory` that does not exist gives the one report line
/// `no config files found in <directory>` and no file. A lock that cannot be
/// taken, as [`take_migration_lock`] states, gives its error and the source
/// file of every migration in `earlier_config_migrations`.
fn migrate_config_directory_with_lock(
    config_directory: &Path,
    earlier_config_migrations: Vec<ConfigFileMigration>,
) -> ConfigMigrationReport {
    match take_migration_lock(config_directory) {
        // `_migration_lock_file` holds the lock until the migration returns.
        Ok(Some(_migration_lock_file)) => migrate_config_directory_with(
            config_directory,
            earlier_config_migrations,
            migrate_config,
            write_atomic,
        ),
        Ok(None) => ConfigMigrationReport {
            config_report_text: Ok(format!(
                "no config files found in {}\n",
                config_directory.display()
            )),
            config_files: Vec::new(),
        },
        Err(lock_error) => ConfigMigrationReport {
            config_report_text: Err(lock_error),
            config_files: list_source_config_files(earlier_config_migrations),
        },
    }
}

/// Opens `<config_directory>/.migration.lock`, creating it with mode `0600` on
/// Unix, and takes its exclusive lock. `Ok(None)` when `config_directory`
/// does not exist.
///
/// # Errors
/// Returns [`CliError::Config`] with `<directory> is not a directory`,
/// `read <directory>: <error>`, `open <lock path>: <error>`, or
/// `lock <lock path>: <error>`.
fn take_migration_lock(config_directory: &Path) -> Result<Option<fs::File>, CliError> {
    match fs::metadata(config_directory) {
        Ok(directory_metadata) if directory_metadata.is_dir() => {}
        Ok(_) => {
            return Err(CliError::Config {
                detail: format!("{} is not a directory", config_directory.display()),
            });
        }
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(read_error) => {
            return Err(CliError::Config {
                detail: format!("read {}: {read_error}", config_directory.display()),
            });
        }
    }
    let migration_lock_path = config_directory.join(".migration.lock");
    let mut migration_lock_options = fs::File::options();
    migration_lock_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        migration_lock_options.mode(0o600);
    }
    let migration_lock_file =
        migration_lock_options
            .open(&migration_lock_path)
            .map_err(|open_error| CliError::Config {
                detail: format!("open {}: {open_error}", migration_lock_path.display()),
            })?;
    migration_lock_file
        .lock()
        .map_err(|lock_error| CliError::Config {
            detail: format!("lock {}: {lock_error}", migration_lock_path.display()),
        })?;
    Ok(Some(migration_lock_file))
}

fn explain_config_key(config_key: &str) -> Result<String, CliError> {
    if let Some(config_field_help) = CONFIG_FIELD_HELP
        .iter()
        .find(|config_field_help| config_field_help.config_key == config_key)
    {
        return Ok(format!(
            "{}\nfile: {}\ndefault: {}\n{}\n",
            config_field_help.config_key,
            config_field_help.config_file_name,
            config_field_help.default_config_value,
            config_field_help.config_field_description
        ));
    }
    let config_keys: Vec<_> = CONFIG_FIELD_HELP
        .iter()
        .map(|config_field_help| config_field_help.config_key)
        .collect();
    Err(CliError::Config {
        detail: format_unknown_key(config_key, &config_keys),
    })
}

/// What validating every config file in one directory produced.
pub(crate) struct ConfigReport {
    /// One line per file that validated, in path order:
    /// `"/home/user/.config/koshi/koshi.kdl: valid (version 2)"` for a file on
    /// this build's schema, and
    /// `"/home/user/.config/koshi/koshi.kdl: valid (version 1; migrate to version 2)"`
    /// for one on an older schema.
    pub(crate) config_report_lines: Vec<String>,
    /// One message per file that could not be read or did not validate.
    pub(crate) config_file_errors: Vec<String>,
}

/// Validate every known config file under `config_directory`: `koshi.kdl`
/// from what the read at start found, `app_config_outcome`, as
/// [`validate_app_config_outcome`] states, and every other file as read from
/// disk.
///
/// Writes nothing and parses no `koshi.kdl` text. `config_file_errors` lists
/// the directory and file read errors first, then the validation errors, each
/// group in path order. A directory with no config file gives empty
/// `config_report_lines` and `config_file_errors`.
pub(crate) fn validate_config_directory(
    config_directory: &Path,
    app_config_outcome: &AppConfigOutcome,
) -> ConfigReport {
    let (config_file_paths, mut config_file_errors) = list_config_file_paths(config_directory);
    let mut config_report_lines = Vec::with_capacity(config_file_paths.len());
    let mut validation_errors = Vec::new();
    for (config_file_kind, config_path) in config_file_paths {
        let config_file_validation = match config_file_kind {
            ConfigFileKind::App => validate_app_config_outcome(&config_path, app_config_outcome),
            ConfigFileKind::Keybinding | ConfigFileKind::Theme | ConfigFileKind::Profile => {
                load_config_file(config_file_kind, config_path.clone()).map(|config_file| {
                    config_file.map(|config_file| {
                        validate_config(
                            config_file_kind,
                            &config_file.config_path,
                            &config_file.config_source_text,
                        )
                    })
                })
            }
        };
        match config_file_validation {
            Ok(None) => {}
            Err(read_error) => config_file_errors.push(read_error),
            Ok(Some(Ok(validated_config))) if validated_config.is_current => {
                config_report_lines.push(format!(
                    "{}: valid (version {})",
                    config_path.display(),
                    validated_config.schema_version
                ));
            }
            Ok(Some(Ok(validated_config))) => config_report_lines.push(format!(
                "{}: valid (version {}; migrate to version {})",
                config_path.display(),
                validated_config.schema_version,
                koshi_config::types::SCHEMA_VERSION
            )),
            Ok(Some(Err(validation_error))) => validation_errors.push(validation_error.to_string()),
        }
    }
    config_file_errors.append(&mut validation_errors);
    ConfigReport {
        config_report_lines,
        config_file_errors,
    }
}

fn check_config_directory(
    config_directory: &Path,
    app_config_outcome: &AppConfigOutcome,
) -> Result<String, CliError> {
    let config_report = validate_config_directory(config_directory, app_config_outcome);
    if !config_report.config_file_errors.is_empty() {
        return Err(CliError::Config {
            detail: config_report.config_file_errors.join("\n"),
        });
    }
    let mut config_report_lines = config_report.config_report_lines;
    if config_report_lines.is_empty() {
        config_report_lines.push(format!(
            "no config files found in {}",
            config_directory.display()
        ));
    }
    Ok(config_report_lines.join("\n") + "\n")
}

type ConfigMigrationFunction =
    fn(ConfigFileKind, &ParsedConfigFile) -> Result<MigratedConfig, MigrationError>;

/// Migrate every config file in `config_directory`: read each file, migrate
/// it with `migrate_config_function`, then write each migrated text with
/// `write_atomic_config_file`, in path order.
///
/// A file whose path and text equal those of a migration in
/// `earlier_config_migrations` takes that migration, and its text is not
/// parsed again. Every other file is parsed once and migrated.
///
/// A read error or a migration error stops the migration before any write,
/// with ``migration stopped before writing any file:`` and every error, and
/// gives every file read, as read. A write error stops the migration with
/// ``migration write failed for <path>: <error>``, the files already
/// migrated, and every file but the one whose write failed: the migrated text
/// for a file written, and the text read for the others.
fn migrate_config_directory_with(
    config_directory: &Path,
    earlier_config_migrations: Vec<ConfigFileMigration>,
    migrate_config_function: ConfigMigrationFunction,
    mut write_atomic_config_file: impl FnMut(&Path, &[u8]) -> Result<(), StorageError>,
) -> ConfigMigrationReport {
    let mut earlier_config_migration_by_path: HashMap<PathBuf, ConfigFileMigration> =
        earlier_config_migrations
            .into_iter()
            .map(|config_file_migration| {
                (
                    config_file_migration
                        .source_config_file
                        .get_config_path()
                        .to_path_buf(),
                    config_file_migration,
                )
            })
            .collect();
    let loaded_config_files = load_config_files(config_directory);
    let mut migration_plan = Vec::with_capacity(loaded_config_files.config_files.len());
    let mut refused_config_files = Vec::new();
    let mut migration_errors = loaded_config_files.config_file_read_errors;
    for config_file in loaded_config_files.config_files {
        let unchanged_config_migration = earlier_config_migration_by_path
            .remove(&config_file.config_path)
            .filter(|config_file_migration| {
                config_file_migration.source_config_file.get_source_text()
                    == config_file.config_source_text
            });
        let ConfigFileMigration {
            source_config_file,
            migration_result,
        } = match unchanged_config_migration {
            Some(config_file_migration) => config_file_migration,
            None => ConfigFileMigration::from_source_config_file(
                config_file.config_file_kind,
                ParsedConfigFile::parse(&config_file.config_path, config_file.config_source_text),
                migrate_config_function,
            ),
        };
        match migration_result {
            Ok(migrated_config) => migration_plan.push((
                config_file.config_write_path,
                source_config_file,
                migrated_config,
            )),
            Err(migration_error) => {
                migration_errors.push(migration_error.to_string());
                refused_config_files.push(source_config_file);
            }
        }
    }
    if !migration_errors.is_empty() {
        let mut config_files = refused_config_files;
        config_files.extend(
            migration_plan
                .into_iter()
                .map(|(_, source_config_file, _)| source_config_file),
        );
        return ConfigMigrationReport {
            config_report_text: Err(CliError::Config {
                detail: format!(
                    "migration stopped before writing any file:\n{}",
                    migration_errors.join("\n")
                ),
            }),
            config_files,
        };
    }

    let mut config_report_lines = Vec::with_capacity(migration_plan.len());
    let mut completed_migration_lines = Vec::new();
    let mut config_files = Vec::with_capacity(migration_plan.len());
    let mut planned_migrations = migration_plan.into_iter();
    while let Some((config_write_path, source_config_file, migrated_config)) =
        planned_migrations.next()
    {
        let config_path = source_config_file.get_config_path();
        let Some(migrated_config_file) = migrated_config.migrated_config_file else {
            config_report_lines.push(format!(
                "{}: current (version {})",
                config_path.display(),
                migrated_config.target_schema_version
            ));
            config_files.push(source_config_file);
            continue;
        };
        if let Err(write_error) = write_atomic_config_file(
            &config_write_path,
            migrated_config_file.get_source_text().as_bytes(),
        ) {
            let mut detail = format!(
                "migration write failed for {}: {write_error}\n{} may already contain migrated data; check it before retrying",
                config_path.display(),
                config_path.display()
            );
            if !completed_migration_lines.is_empty() {
                detail.push_str("\nfiles already migrated before this failure:\n");
                detail.push_str(&completed_migration_lines.join("\n"));
            }
            config_files
                .extend(planned_migrations.map(|(_, source_config_file, _)| source_config_file));
            return ConfigMigrationReport {
                config_report_text: Err(CliError::Config { detail }),
                config_files,
            };
        }
        let migration_report_line = format!(
            "{}: migrated version {} to {}",
            config_path.display(),
            migrated_config.source_schema_version,
            migrated_config.target_schema_version
        );
        completed_migration_lines.push(migration_report_line.clone());
        config_report_lines.push(migration_report_line);
        config_files.push(migrated_config_file);
    }
    if config_report_lines.is_empty() {
        config_report_lines.push(format!(
            "no config files found in {}",
            config_directory.display()
        ));
    }
    ConfigMigrationReport {
        config_report_text: Ok(config_report_lines.join("\n") + "\n"),
        config_files,
    }
}

/// Every known config file under `config_directory`, read from disk in path
/// order, and one message per file or directory that could not be read.
fn load_config_files(config_directory: &Path) -> LoadedConfigFiles {
    let (config_file_paths, mut config_file_read_errors) = list_config_file_paths(config_directory);
    let mut config_files = Vec::with_capacity(config_file_paths.len());
    for (config_file_kind, config_path) in config_file_paths {
        match load_config_file(config_file_kind, config_path) {
            Ok(Some(config_file)) => config_files.push(config_file),
            Ok(None) => {}
            Err(read_error) => config_file_read_errors.push(read_error),
        }
    }
    LoadedConfigFiles {
        config_files,
        config_file_read_errors,
    }
}

/// Every known config file path under `config_directory` with its kind,
/// sorted by path: `keybinding.kdl` and `koshi.kdl` whether or not they exist,
/// and every `*.kdl` entry of `themes/` and `profile/`. Also gives one
/// `read <directory>: <error>` message per `themes` or `profile` directory, or
/// entry of one, that could not be read.
fn list_config_file_paths(
    config_directory: &Path,
) -> (Vec<(ConfigFileKind, PathBuf)>, Vec<String>) {
    let mut config_file_paths = vec![
        (ConfigFileKind::App, config_directory.join("koshi.kdl")),
        (
            ConfigFileKind::Keybinding,
            config_directory.join("keybinding.kdl"),
        ),
    ];
    let mut config_file_read_errors = Vec::new();
    append_kdl_file_paths(
        &mut config_file_paths,
        ConfigFileKind::Theme,
        &config_directory.join("themes"),
        &mut config_file_read_errors,
    );
    append_kdl_file_paths(
        &mut config_file_paths,
        ConfigFileKind::Profile,
        &config_directory.join("profile"),
        &mut config_file_read_errors,
    );
    config_file_paths.sort_by(|left_config_file, right_config_file| {
        left_config_file.1.cmp(&right_config_file.1)
    });
    (config_file_paths, config_file_read_errors)
}

fn load_config_file(
    config_file_kind: ConfigFileKind,
    config_path: PathBuf,
) -> Result<Option<ConfigFile>, String> {
    let Some(config_write_path) = resolve_config_write_path(&config_path)? else {
        return Ok(None);
    };
    let config_source_text = fs::read_to_string(&config_path)
        .map_err(|io_error| format!("read {}: {io_error}", config_path.display()))?;
    Ok(Some(ConfigFile {
        config_file_kind,
        config_path,
        config_write_path,
        config_source_text,
    }))
}

/// The validation of `koshi.kdl` at `config_path` from `app_config_outcome`,
/// what the read at start found. Parses no text.
///
/// The path is checked as [`resolve_config_write_path`] states: nothing at the
/// path gives `Ok(None)`, and a path that is no regular file gives its
/// `read <path>: …` message as `Err`. Then [`AppConfigOutcome::Absent`] gives
/// `Ok(None)`, [`AppConfigOutcome::Unreadable`] gives
/// `Err("read <path>: <read error>")`, and a refused or parsed file gives
/// `Ok(Some(_))` with what [`validate_parsed_app_config`] gives for it.
fn validate_app_config_outcome(
    config_path: &Path,
    app_config_outcome: &AppConfigOutcome,
) -> Result<Option<Result<ValidatedConfig, MigrationError>>, String> {
    if resolve_config_write_path(config_path)?.is_none() {
        return Ok(None);
    }
    let app_config_result = match app_config_outcome {
        AppConfigOutcome::Absent => return Ok(None),
        AppConfigOutcome::Unreadable { read_error_text } => {
            return Err(format!("read {}: {read_error_text}", config_path.display()));
        }
        AppConfigOutcome::Refused { config_error, .. } => Err(config_error),
        AppConfigOutcome::Parsed {
            app_config_file, ..
        } => Ok(app_config_file.as_ref()),
    };
    Ok(Some(validate_parsed_app_config(
        config_path,
        app_config_result,
    )))
}

/// The path a migration writes `config_path` through: the link target for a
/// symbolic link, and `config_path` itself otherwise. `Ok(None)` when nothing
/// is at `config_path`. `read <path>: <error>` when the path cannot be
/// inspected or a link cannot be followed, and
/// `read <path>: expected a regular file` when the path, or the link target,
/// is not a regular file.
fn resolve_config_write_path(config_path: &Path) -> Result<Option<PathBuf>, String> {
    let link_metadata = match fs::symlink_metadata(config_path) {
        Ok(link_metadata) => link_metadata,
        Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(io_error) => return Err(format!("read {}: {io_error}", config_path.display())),
    };
    let (config_file_metadata, config_write_path) = if link_metadata.file_type().is_symlink() {
        let config_write_path = fs::canonicalize(config_path)
            .map_err(|io_error| format!("read {}: {io_error}", config_path.display()))?;
        let config_file_metadata = fs::metadata(config_path)
            .map_err(|io_error| format!("read {}: {io_error}", config_path.display()))?;
        (config_file_metadata, config_write_path)
    } else {
        (link_metadata, config_path.to_path_buf())
    };
    if !config_file_metadata.is_file() {
        return Err(format!(
            "read {}: expected a regular file",
            config_path.display()
        ));
    }
    Ok(Some(config_write_path))
}

fn append_kdl_file_paths(
    config_file_paths: &mut Vec<(ConfigFileKind, PathBuf)>,
    config_file_kind: ConfigFileKind,
    config_directory: &Path,
    config_file_read_errors: &mut Vec<String>,
) {
    let directory_entries = match fs::read_dir(config_directory) {
        Ok(directory_entries) => directory_entries,
        Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => return,
        Err(io_error) => {
            config_file_read_errors
                .push(format!("read {}: {io_error}", config_directory.display()));
            return;
        }
    };
    for directory_entry_result in directory_entries {
        let directory_entry = match directory_entry_result {
            Ok(directory_entry) => directory_entry,
            Err(io_error) => {
                config_file_read_errors
                    .push(format!("read {}: {io_error}", config_directory.display()));
                continue;
            }
        };
        let config_file_path = directory_entry.path();
        if config_file_path.extension() == Some(OsStr::new("kdl")) {
            config_file_paths.push((config_file_kind, config_file_path));
        }
    }
}
