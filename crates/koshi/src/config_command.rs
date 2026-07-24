//! Local `koshi config` command implementation.
//!
//! The command scans only Koshi's known config paths: `koshi.kdl`,
//! `keybinding.kdl`, `themes/*.kdl`, and `profile/*.kdl`. Validation reads
//! every present regular file and reports all read and schema errors together.
//! Migration keeps config symlinks, validates every result in memory, then
//! atomically replaces each changed file or symlink target.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use koshi_config::migration::{
    migrate_config, validate_config, ConfigFileKind, MigratedConfig, MigrationError,
};
use koshi_config::parser::unknown_key;
use koshi_storage::atomic::write_atomic;

use crate::cli::ConfigCommand;
use crate::error::CliError;

#[cfg(test)]
mod tests;

struct ConfigFile {
    kind: ConfigFileKind,
    path: PathBuf,
    write_path: PathBuf,
    source: String,
}

struct ConfigFiles {
    files: Vec<ConfigFile>,
    errors: Vec<String>,
}

struct FieldHelp {
    key: &'static str,
    file: &'static str,
    default: &'static str,
    description: &'static str,
}

const FIELDS: &[FieldHelp] = &[
    field("koshi.version", "koshi.kdl", "1", "Config schema version."),
    field(
        "koshi.theme",
        "koshi.kdl",
        "\"default\"",
        "Color theme file name.",
    ),
    field(
        "koshi.pane.min-cols",
        "koshi.kdl",
        "2",
        "Smallest pane width in columns.",
    ),
    field(
        "koshi.pane.min-rows",
        "koshi.kdl",
        "1",
        "Smallest pane height in rows.",
    ),
    field(
        "koshi.scrollback.max-lines",
        "koshi.kdl",
        "10000",
        "Most saved lines per pane.",
    ),
    field(
        "koshi.scrollback.max-bytes",
        "koshi.kdl",
        "33554432",
        "Most saved text bytes per pane.",
    ),
    field(
        "koshi.scrollback.scroll-on-input",
        "koshi.kdl",
        "#true",
        "Jump to newest output after input.",
    ),
    field(
        "koshi.layout.new-pane-direction",
        "koshi.kdl",
        "\"right\"",
        "Default side for a new pane.",
    ),
    field(
        "koshi.mouse.border-resize",
        "koshi.kdl",
        "#true",
        "Allow border dragging to resize panes.",
    ),
    field(
        "koshi.mouse.scroll-lines",
        "koshi.kdl",
        "3",
        "Lines moved by one wheel step.",
    ),
    field(
        "koshi.mouse.wheel",
        "koshi.kdl",
        "\"scroll-scrollback\"",
        "Wheel action over a plain pane.",
    ),
    field(
        "koshi.copy.trim-trailing-whitespace",
        "koshi.kdl",
        "#true",
        "Trim line-end spaces when copying.",
    ),
    field(
        "koshi.terminal.term",
        "koshi.kdl",
        "\"xterm-256color\"",
        "TERM value given to child programs.",
    ),
    field(
        "koshi.terminal.colorterm",
        "koshi.kdl",
        "\"truecolor\"",
        "COLORTERM value given to child programs.",
    ),
    field(
        "koshi.terminal.default-shell",
        "koshi.kdl",
        "$SHELL",
        "Shell used for a new terminal pane.",
    ),
    field(
        "koshi.logging.enabled",
        "koshi.kdl",
        "#false",
        "Write Koshi log files.",
    ),
    field(
        "koshi.logging.level",
        "koshi.kdl",
        "\"warning\"",
        "Lowest log level written.",
    ),
    field(
        "koshi.logging.format",
        "koshi.kdl",
        "\"pretty\"",
        "Human or JSON log format.",
    ),
    field(
        "koshi.update.auto-check",
        "koshi.kdl",
        "#true",
        "Check for updates on interactive start.",
    ),
    field(
        "koshi.update.check-interval-days",
        "koshi.kdl",
        "14",
        "Days between startup update checks.",
    ),
    field(
        "koshi.update.allow-prerelease",
        "koshi.kdl",
        "#false",
        "Include prerelease builds in update checks.",
    ),
    field(
        "keybinding.version",
        "keybinding.kdl",
        "1",
        "Config schema version.",
    ),
    field(
        "keybinding.chord-timeout-ms",
        "keybinding.kdl",
        "500",
        "Wait for the next key in a sequence.",
    ),
    field(
        "keybinding.which-key-delay-ms",
        "keybinding.kdl",
        "300",
        "Wait before showing key hints.",
    ),
    field(
        "keybinding.max-chord-depth",
        "keybinding.kdl",
        "4",
        "Most keys in one sequence.",
    ),
    field(
        "keybinding.leader",
        "keybinding.kdl",
        "\"C-\"",
        "Prefix used by `<leader>` bindings.",
    ),
    field(
        "keybinding.unlock-alternative",
        "keybinding.kdl",
        "unset",
        "Replacement key that always unlocks input.",
    ),
    field(
        "theme.version",
        "themes/<name>.kdl",
        "1",
        "Config schema version.",
    ),
    field(
        "theme.colors.ramp-start",
        "themes/<name>.kdl",
        "\"#d0a5ff\"",
        "First chrome gradient color.",
    ),
    field(
        "theme.colors.ramp-end",
        "themes/<name>.kdl",
        "\"#7dbcff\"",
        "Last chrome gradient color.",
    ),
    field(
        "theme.colors.on-ramp",
        "themes/<name>.kdl",
        "\"#12091f\"",
        "Text over the chrome gradient.",
    ),
    field(
        "theme.colors.on-ramp-dim",
        "themes/<name>.kdl",
        "\"#f0ecfa\"",
        "Dim text over the chrome gradient.",
    ),
    field(
        "theme.colors.accent",
        "themes/<name>.kdl",
        "\"#f5c2ff\"",
        "Color for a key sequence in progress.",
    ),
    field(
        "theme.colors.on-accent",
        "themes/<name>.kdl",
        "\"#1e1033\"",
        "Text over the accent color.",
    ),
    field(
        "theme.colors.bar-bg",
        "themes/<name>.kdl",
        "\"#000000\"",
        "Tab and key-hint bar background.",
    ),
    field(
        "theme.colors.border-focused",
        "themes/<name>.kdl",
        "\"#00afd7\"",
        "Focused pane border.",
    ),
    field(
        "theme.colors.border-unfocused",
        "themes/<name>.kdl",
        "\"#585858\"",
        "Unfocused pane border.",
    ),
    field(
        "theme.colors.border-hover",
        "themes/<name>.kdl",
        "\"#af5fff\"",
        "Pane border under the pointer.",
    ),
    field(
        "theme.colors.stack-header-fg",
        "themes/<name>.kdl",
        "\"#f4f1fa\"",
        "Collapsed stack header text.",
    ),
    field(
        "theme.colors.stack-header-bg",
        "themes/<name>.kdl",
        "\"#300f4a\"",
        "Collapsed stack header background.",
    ),
    field(
        "theme.colors.letterbox",
        "themes/<name>.kdl",
        "\"#585858\"",
        "Margin around a centered layout.",
    ),
    field(
        "profile.version",
        "profile/<name>.kdl",
        "1",
        "Config schema version.",
    ),
];

const fn field(
    key: &'static str,
    file: &'static str,
    default: &'static str,
    description: &'static str,
) -> FieldHelp {
    FieldHelp {
        key,
        file,
        default,
        description,
    }
}

/// Runs one config command and prints its result.
///
/// # Errors
/// Returns [`CliError::Config`] when the platform has no config directory or
/// a file cannot be read, validated, explained, or replaced.
pub fn run(command: &ConfigCommand) -> Result<(), CliError> {
    let dir = koshi_paths::config_dir().ok_or_else(|| CliError::Config {
        detail: "platform config directory is unavailable".to_string(),
    })?;
    let output = run_in_dir(command, &dir)?;
    print!("{output}");
    Ok(())
}

fn run_in_dir(command: &ConfigCommand, dir: &Path) -> Result<String, CliError> {
    match command {
        ConfigCommand::Path => Ok(format!("{}\n", dir.display())),
        ConfigCommand::Explain { key } => explain(key),
        ConfigCommand::Check => check(dir),
        ConfigCommand::Migrate => migrate_in_dir_with(dir, migrate_config),
    }
}

fn explain(key: &str) -> Result<String, CliError> {
    if let Some(field) = FIELDS.iter().find(|field| field.key == key) {
        return Ok(format!(
            "{}\nfile: {}\ndefault: {}\n{}\n",
            field.key, field.file, field.default, field.description
        ));
    }
    let keys: Vec<_> = FIELDS.iter().map(|field| field.key).collect();
    Err(CliError::Config {
        detail: unknown_key(key, &keys),
    })
}

fn check(dir: &Path) -> Result<String, CliError> {
    let loaded = read_files(dir);
    let mut lines = Vec::with_capacity(loaded.files.len());
    let mut errors = loaded.errors;
    for file in &loaded.files {
        match validate_config(file.kind, &file.path, &file.source) {
            Ok(validated) if validated.current => lines.push(format!(
                "{}: valid (version {})",
                file.path.display(),
                validated.version
            )),
            Ok(validated) => lines.push(format!(
                "{}: valid (version {}; migrate to version {})",
                file.path.display(),
                validated.version,
                koshi_config::types::SCHEMA_VERSION
            )),
            Err(error) => errors.push(error.to_string()),
        }
    }
    if !errors.is_empty() {
        return Err(CliError::Config {
            detail: errors.join("\n"),
        });
    }
    if lines.is_empty() {
        lines.push(format!("no config files found in {}", dir.display()));
    }
    Ok(lines.join("\n") + "\n")
}

type MigrateFn = fn(ConfigFileKind, &Path, &str) -> Result<MigratedConfig, MigrationError>;

fn migrate_in_dir_with(dir: &Path, migrate: MigrateFn) -> Result<String, CliError> {
    let loaded = read_files(dir);
    let mut planned = Vec::with_capacity(loaded.files.len());
    let mut errors = loaded.errors;
    for file in loaded.files {
        match migrate(file.kind, &file.path, &file.source) {
            Ok(result) => planned.push((file.path, file.write_path, result)),
            Err(error) => errors.push(error.to_string()),
        }
    }
    if !errors.is_empty() {
        return Err(CliError::Config {
            detail: format!(
                "migration stopped before writing any file:\n{}",
                errors.join("\n")
            ),
        });
    }

    let mut lines = Vec::with_capacity(planned.len());
    for (path, write_path, result) in planned {
        if result.changed {
            write_atomic(&write_path, result.source.as_bytes()).map_err(|error| {
                CliError::Config {
                    detail: format!("replace {}: {error}", path.display()),
                }
            })?;
            lines.push(format!(
                "{}: migrated version {} to {}",
                path.display(),
                result.from,
                result.to
            ));
        } else {
            lines.push(format!(
                "{}: current (version {})",
                path.display(),
                result.to
            ));
        }
    }
    if lines.is_empty() {
        lines.push(format!("no config files found in {}", dir.display()));
    }
    Ok(lines.join("\n") + "\n")
}

fn read_files(dir: &Path) -> ConfigFiles {
    let mut paths = vec![
        (ConfigFileKind::App, dir.join("koshi.kdl")),
        (ConfigFileKind::Keybinding, dir.join("keybinding.kdl")),
    ];
    let mut errors = Vec::new();
    push_kdl_files(
        &mut paths,
        ConfigFileKind::Theme,
        &dir.join("themes"),
        &mut errors,
    );
    push_kdl_files(
        &mut paths,
        ConfigFileKind::Profile,
        &dir.join("profile"),
        &mut errors,
    );
    paths.sort_by(|left, right| left.1.cmp(&right.1));

    let mut files = Vec::with_capacity(paths.len());
    for (kind, path) in paths {
        match read_file(kind, path) {
            Ok(Some(file)) => files.push(file),
            Ok(None) => {}
            Err(error) => errors.push(error),
        }
    }
    ConfigFiles { files, errors }
}

fn read_file(kind: ConfigFileKind, path: PathBuf) -> Result<Option<ConfigFile>, String> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let (metadata, write_path) = if metadata.file_type().is_symlink() {
        let write_path =
            fs::canonicalize(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let metadata =
            fs::metadata(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        (metadata, write_path)
    } else {
        (metadata, path.clone())
    };
    if !metadata.is_file() {
        return Err(format!("read {}: expected a regular file", path.display()));
    }
    let source =
        fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(Some(ConfigFile {
        kind,
        path,
        write_path,
        source,
    }))
}

fn push_kdl_files(
    paths: &mut Vec<(ConfigFileKind, PathBuf)>,
    kind: ConfigFileKind,
    dir: &Path,
    errors: &mut Vec<String>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            errors.push(format!("read {}: {error}", dir.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("read {}: {error}", dir.display()));
                continue;
            }
        };
        let path = entry.path();
        if path.extension() == Some(OsStr::new("kdl")) {
            paths.push((kind, path));
        }
    }
}
