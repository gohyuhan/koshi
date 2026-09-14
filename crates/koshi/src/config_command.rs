//! Local `koshi config` command implementation.
//!
//! The command scans only Koshi's known config paths: `koshi.kdl`,
//! `keybinding.kdl`, `themes/*.kdl`, and `profile/*.kdl`. Validation reads
//! every present regular file and reports all read and schema errors together.
//! Migration keeps config symlinks, validates every result in memory, then
//! atomically replaces each changed file or symlink target. A write failure
//! names files already migrated and marks the failing file as possibly changed.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use koshi_config::migration::{
    migrate_config, validate_config, ConfigFileKind, MigratedConfig, MigrationError,
};
use koshi_config::parser::format_unknown_key;
use koshi_storage::{atomic::write_atomic, error::StorageError};

use crate::cli::ConfigCommand;
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
    read_errors: Vec<String>,
}

struct ConfigFieldHelp {
    config_key: &'static str,
    config_file_name: &'static str,
    default_value: &'static str,
    description: &'static str,
}

const CONFIG_FIELD_HELP: &[ConfigFieldHelp] = &[
    build_config_field_help("koshi.version", "koshi.kdl", "1", "Config schema version."),
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
        "Let other users of this machine reach your sessions.",
    ),
    build_config_field_help(
        "koshi.shared-sessions-dir",
        "koshi.kdl",
        "\"/tmp/koshi\", %ProgramData%\\koshi on Windows",
        "Directory the shared session sockets live in.",
    ),
    build_config_field_help(
        "koshi.remote-listen",
        "koshi.kdl",
        "unset",
        "Address the remote listener binds to. Setting it opens nothing; \
         `koshi share grant` asks before the port opens.",
    ),
    build_config_field_help(
        "koshi.auto-close-session",
        "koshi.kdl",
        "#false",
        "End the session when its last client leaves.",
    ),
    build_config_field_help(
        "koshi.remote-reconnect",
        "koshi.kdl",
        "#true",
        "Dial a session on another machine again when the link drops.",
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
        "$SHELL",
        "Shell used for a new terminal pane.",
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
        "1",
        "Config schema version.",
    ),
    build_config_field_help(
        "keybinding.chord-timeout-ms",
        "keybinding.kdl",
        "500",
        "Wait for the next key in a sequence.",
    ),
    build_config_field_help(
        "keybinding.which-key-delay-ms",
        "keybinding.kdl",
        "300",
        "Wait before showing key hints.",
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
        "1",
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
        "1",
        "Config schema version.",
    ),
];

const fn build_config_field_help(
    config_key: &'static str,
    config_file_name: &'static str,
    default_value: &'static str,
    description: &'static str,
) -> ConfigFieldHelp {
    ConfigFieldHelp {
        config_key,
        config_file_name,
        default_value,
        description,
    }
}

/// Runs one config command and prints its result.
///
/// # Errors
/// Returns [`CliError::Config`] when the platform has no config directory or
/// a file cannot be read, validated, explained, or replaced.
pub fn run_config_command(command: &ConfigCommand) -> Result<(), CliError> {
    let config_directory =
        koshi_paths::resolve_config_directory().ok_or_else(|| CliError::Config {
            detail: "platform config directory is unavailable".to_string(),
        })?;
    let command_output = run_config_command_in_directory(command, &config_directory)?;
    print!("{command_output}");
    Ok(())
}

fn run_config_command_in_directory(
    command: &ConfigCommand,
    config_directory: &Path,
) -> Result<String, CliError> {
    match command {
        ConfigCommand::Path => Ok(format!("{}\n", config_directory.display())),
        ConfigCommand::Explain { config_key } => explain_config_key(config_key),
        ConfigCommand::Check => check_config_directory(config_directory),
        ConfigCommand::Migrate => {
            migrate_config_directory_with(config_directory, migrate_config, write_atomic)
        }
    }
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
            config_field_help.default_value,
            config_field_help.description
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
    /// `"/home/u/.config/koshi/koshi.kdl: valid (version 1)"` for a file on
    /// this build's schema, and
    /// `"/home/u/.config/koshi/koshi.kdl: valid (version 1; migrate to version 2)"`
    /// for one on an older schema.
    pub(crate) report_lines: Vec<String>,
    /// One message per file that could not be read or did not validate.
    pub(crate) validation_errors: Vec<String>,
}

/// Read and validate every known config file under `dir`.
///
/// Reads the filesystem and writes nothing. A directory with no config file
/// gives empty `lines` and empty `errors`.
pub(crate) fn validate_config_directory(config_directory: &Path) -> ConfigReport {
    let loaded_config_files = load_config_files(config_directory);
    let mut report_lines = Vec::with_capacity(loaded_config_files.config_files.len());
    let mut validation_errors = loaded_config_files.read_errors;
    for config_file in &loaded_config_files.config_files {
        match validate_config(
            config_file.config_file_kind,
            &config_file.config_path,
            &config_file.config_source_text,
        ) {
            Ok(validated) if validated.is_current => report_lines.push(format!(
                "{}: valid (version {})",
                config_file.config_path.display(),
                validated.schema_version
            )),
            Ok(validated) => report_lines.push(format!(
                "{}: valid (version {}; migrate to version {})",
                config_file.config_path.display(),
                validated.schema_version,
                koshi_config::types::SCHEMA_VERSION
            )),
            Err(config_error) => validation_errors.push(config_error.to_string()),
        }
    }
    ConfigReport {
        report_lines,
        validation_errors,
    }
}

fn check_config_directory(config_directory: &Path) -> Result<String, CliError> {
    let config_report = validate_config_directory(config_directory);
    if !config_report.validation_errors.is_empty() {
        return Err(CliError::Config {
            detail: config_report.validation_errors.join("\n"),
        });
    }
    let mut report_lines = config_report.report_lines;
    if report_lines.is_empty() {
        report_lines.push(format!(
            "no config files found in {}",
            config_directory.display()
        ));
    }
    Ok(report_lines.join("\n") + "\n")
}

type ConfigMigrationFunction =
    fn(ConfigFileKind, &Path, &str) -> Result<MigratedConfig, MigrationError>;

fn migrate_config_directory_with(
    config_directory: &Path,
    migrate_config_function: ConfigMigrationFunction,
    mut write_atomic_config_file: impl FnMut(&Path, &[u8]) -> Result<(), StorageError>,
) -> Result<String, CliError> {
    let loaded_config_files = load_config_files(config_directory);
    let mut migration_plan = Vec::with_capacity(loaded_config_files.config_files.len());
    let mut migration_errors = loaded_config_files.read_errors;
    for config_file in loaded_config_files.config_files {
        match migrate_config_function(
            config_file.config_file_kind,
            &config_file.config_path,
            &config_file.config_source_text,
        ) {
            Ok(migrated_config) => migration_plan.push((
                config_file.config_path,
                config_file.config_write_path,
                migrated_config,
            )),
            Err(migration_error) => migration_errors.push(migration_error.to_string()),
        }
    }
    if !migration_errors.is_empty() {
        return Err(CliError::Config {
            detail: format!(
                "migration stopped before writing any file:\n{}",
                migration_errors.join("\n")
            ),
        });
    }

    let mut report_lines = Vec::with_capacity(migration_plan.len());
    let mut completed_migration_lines = Vec::new();
    for (config_path, config_write_path, migrated_config) in migration_plan {
        let migration_report_line = if migrated_config.is_changed {
            if let Err(write_error) = write_atomic_config_file(
                &config_write_path,
                migrated_config.migrated_source.as_bytes(),
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
                return Err(CliError::Config { detail });
            }
            let migration_report_line = format!(
                "{}: migrated version {} to {}",
                config_path.display(),
                migrated_config.source_schema_version,
                migrated_config.target_schema_version
            );
            completed_migration_lines.push(migration_report_line.clone());
            migration_report_line
        } else {
            format!(
                "{}: current (version {})",
                config_path.display(),
                migrated_config.target_schema_version
            )
        };
        report_lines.push(migration_report_line);
    }
    if report_lines.is_empty() {
        report_lines.push(format!(
            "no config files found in {}",
            config_directory.display()
        ));
    }
    Ok(report_lines.join("\n") + "\n")
}

fn load_config_files(config_directory: &Path) -> LoadedConfigFiles {
    let mut config_file_paths = vec![
        (ConfigFileKind::App, config_directory.join("koshi.kdl")),
        (
            ConfigFileKind::Keybinding,
            config_directory.join("keybinding.kdl"),
        ),
    ];
    let mut read_errors = Vec::new();
    append_kdl_file_paths(
        &mut config_file_paths,
        ConfigFileKind::Theme,
        &config_directory.join("themes"),
        &mut read_errors,
    );
    append_kdl_file_paths(
        &mut config_file_paths,
        ConfigFileKind::Profile,
        &config_directory.join("profile"),
        &mut read_errors,
    );
    config_file_paths.sort_by(|left, right| left.1.cmp(&right.1));

    let mut config_files = Vec::with_capacity(config_file_paths.len());
    for (config_file_kind, config_path) in config_file_paths {
        match load_config_file(config_file_kind, config_path) {
            Ok(Some(config_file)) => config_files.push(config_file),
            Ok(None) => {}
            Err(read_error) => read_errors.push(read_error),
        }
    }
    LoadedConfigFiles {
        config_files,
        read_errors,
    }
}

fn load_config_file(
    config_file_kind: ConfigFileKind,
    config_path: PathBuf,
) -> Result<Option<ConfigFile>, String> {
    let link_metadata = match fs::symlink_metadata(&config_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", config_path.display())),
    };
    let (config_file_metadata, config_write_path) = if link_metadata.file_type().is_symlink() {
        let config_write_path = fs::canonicalize(&config_path)
            .map_err(|error| format!("read {}: {error}", config_path.display()))?;
        let config_file_metadata = fs::metadata(&config_path)
            .map_err(|error| format!("read {}: {error}", config_path.display()))?;
        (config_file_metadata, config_write_path)
    } else {
        (link_metadata, config_path.clone())
    };
    if !config_file_metadata.is_file() {
        return Err(format!(
            "read {}: expected a regular file",
            config_path.display()
        ));
    }
    let config_source_text = fs::read_to_string(&config_path)
        .map_err(|error| format!("read {}: {error}", config_path.display()))?;
    Ok(Some(ConfigFile {
        config_file_kind,
        config_path,
        config_write_path,
        config_source_text,
    }))
}

fn append_kdl_file_paths(
    config_file_paths: &mut Vec<(ConfigFileKind, PathBuf)>,
    config_file_kind: ConfigFileKind,
    config_directory: &Path,
    read_errors: &mut Vec<String>,
) {
    let directory_entries = match fs::read_dir(config_directory) {
        Ok(directory_entries) => directory_entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            read_errors.push(format!("read {}: {error}", config_directory.display()));
            return;
        }
    };
    for directory_entry_result in directory_entries {
        let directory_entry = match directory_entry_result {
            Ok(directory_entry) => directory_entry,
            Err(error) => {
                read_errors.push(format!("read {}: {error}", config_directory.display()));
                continue;
            }
        };
        let config_file_path = directory_entry.path();
        if config_file_path.extension() == Some(OsStr::new("kdl")) {
            config_file_paths.push((config_file_kind, config_file_path));
        }
    }
}
