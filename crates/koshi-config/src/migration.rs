//! Config schema validation and ordered migration.
//!
//! Each supported schema version owns one validator and, except the newest
//! version, one migration to the next version. A file is validated before its
//! first migration and after every step. Migration stops on the first bad
//! source file, bad result, or missing step.

use std::path::Path;

use kdl::KdlDocument;
use thiserror::Error;

use crate::app_config::parse_app_config;
use crate::error::{ConfigError, ConfigParseDiagnostic};
use crate::keybinding::{parse_keybindings, KeybindingParseError};
use crate::parser::parse_kdl;
use crate::profile::{parse_profile, ProfileError};
use crate::theme::parse_theme;
use crate::types::SCHEMA_VERSION;

#[cfg(test)]
mod tests;

/// Config file schema selected from its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFileKind {
    /// Main `koshi.kdl` settings.
    App,
    /// `keybinding.kdl` key settings.
    Keybinding,
    /// One file below `themes/`.
    Theme,
    /// One file below `profile/`.
    Profile,
}

/// Successful validation of one config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedConfig {
    /// Schema version declared by the file.
    pub version: u32,
    /// Whether the file already uses this build's newest schema.
    pub current: bool,
}

/// Successful migration of one config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigratedConfig {
    /// Schema version declared before migration.
    pub from: u32,
    /// Schema version declared after migration.
    pub to: u32,
    /// Migrated KDL text, or the original text when no migration was needed.
    pub source: String,
    /// Whether at least one migration ran.
    pub changed: bool,
}

/// Config validation or migration failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MigrationError {
    /// KDL text could not be parsed.
    #[error("{detail}")]
    Parse {
        /// Full parse diagnostic as plain text.
        detail: String,
    },
    /// The file did not declare one usable schema version.
    #[error("invalid config version in {path}: {detail}")]
    Version {
        /// File path shown to the user.
        path: String,
        /// Plain reason the version cannot be used.
        detail: String,
    },
    /// The file does not match its declared schema.
    #[error("invalid config file {path}: {details}")]
    Invalid {
        /// File path shown to the user.
        path: String,
        /// All schema problems joined for terminal output.
        details: String,
    },
    /// Koshi has no schema implementation for a declared version.
    #[error("config schema version {version} has no validator in this koshi build")]
    MissingSchema {
        /// Version with no registered schema.
        version: u32,
    },
    /// Koshi has no migration between two adjacent supported versions.
    #[error("no config migration from version {from} to version {to}")]
    MissingStep {
        /// Source version.
        from: u32,
        /// Required next version.
        to: u32,
    },
}

type ValidateFn = fn(ConfigFileKind, &Path, &str) -> Result<(), MigrationError>;
type MigrateFn = fn(&Path, &str) -> Result<String, MigrationError>;

#[derive(Clone, Copy)]
struct Schema {
    version: u32,
    validate: ValidateFn,
    migrate_to_next: Option<MigrateFn>,
}

const SCHEMAS: &[Schema] = &[Schema {
    version: 1,
    validate: validate_v1,
    migrate_to_next: None,
}];

/// Validates one config file against the schema version it declares.
///
/// # Errors
/// Returns [`MigrationError`] for bad KDL, a missing or unusable version,
/// an unknown schema version, or any schema error.
pub fn validate_config(
    kind: ConfigFileKind,
    path: &Path,
    source: &str,
) -> Result<ValidatedConfig, MigrationError> {
    check_registry(SCHEMAS)?;
    let version = read_version(path, source)?;
    if version > SCHEMA_VERSION {
        return Err(MigrationError::Version {
            path: path.display().to_string(),
            detail: format!(
                "schema version {version} is newer than this koshi supports ({SCHEMA_VERSION})"
            ),
        });
    }
    let schema = schema_for(SCHEMAS, version)?;
    (schema.validate)(kind, path, source)?;
    Ok(ValidatedConfig {
        version,
        current: version == SCHEMA_VERSION,
    })
}

/// Migrates one valid config through every adjacent schema version.
///
/// # Errors
/// Returns [`MigrationError`] before producing output when the source is bad,
/// a schema or migration step is missing, or any migrated result is invalid.
pub fn migrate_config(
    kind: ConfigFileKind,
    path: &Path,
    source: &str,
) -> Result<MigratedConfig, MigrationError> {
    migrate_with_registry(kind, path, source, SCHEMAS, SCHEMA_VERSION)
}

fn migrate_with_registry(
    kind: ConfigFileKind,
    path: &Path,
    source: &str,
    schemas: &[Schema],
    current: u32,
) -> Result<MigratedConfig, MigrationError> {
    check_registry_for(schemas, current)?;
    let from = read_version(path, source)?;
    if from > current {
        return Err(MigrationError::Version {
            path: path.display().to_string(),
            detail: format!("schema version {from} is newer than this koshi supports ({current})"),
        });
    }

    let mut version = from;
    let mut migrated = source.to_string();
    loop {
        let schema = schema_for(schemas, version)?;
        (schema.validate)(kind, path, &migrated)?;
        if version == current {
            break;
        }
        let next = version + 1;
        let migrate = schema.migrate_to_next.ok_or(MigrationError::MissingStep {
            from: version,
            to: next,
        })?;
        migrated = migrate(path, &migrated)?;
        let declared = read_version(path, &migrated)?;
        if declared != next {
            return Err(MigrationError::Version {
                path: path.display().to_string(),
                detail: format!(
                    "migration from version {version} produced version {declared}, expected {next}"
                ),
            });
        }
        version = next;
    }

    Ok(MigratedConfig {
        from,
        to: version,
        changed: from != version,
        source: migrated,
    })
}

fn read_version(path: &Path, source: &str) -> Result<u32, MigrationError> {
    let doc = parse_kdl(path, source).map_err(parse_error)?;
    version_from_document(path, &doc)
}

fn version_from_document(path: &Path, doc: &KdlDocument) -> Result<u32, MigrationError> {
    let mut versions = doc
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "version");
    let Some(node) = versions.next() else {
        return Err(version_error(path, "file must declare `version`"));
    };
    if versions.next().is_some() {
        return Err(version_error(path, "`version` is declared more than once"));
    }
    if node.children().is_some() {
        return Err(version_error(path, "`version` takes no children"));
    }
    let [entry] = node.entries() else {
        return Err(version_error(
            path,
            "`version` takes exactly one integer argument",
        ));
    };
    if entry.name().is_some() {
        return Err(version_error(
            path,
            "`version` takes an argument, not a property",
        ));
    }
    let Some(value) = entry.value().as_integer() else {
        return Err(version_error(path, "`version` must be an integer"));
    };
    let version = u32::try_from(value)
        .map_err(|_| version_error(path, "`version` must be between 1 and 4294967295"))?;
    if version == 0 {
        return Err(version_error(path, "`version` must be at least 1"));
    }
    Ok(version)
}

fn validate_v1(kind: ConfigFileKind, path: &Path, source: &str) -> Result<(), MigrationError> {
    match kind {
        ConfigFileKind::App => {
            let parsed =
                parse_app_config(path, source).map_err(|error| config_error(path, error))?;
            invalid_warnings(path, parsed.warnings)
        }
        ConfigFileKind::Theme => {
            let (_, warnings) =
                parse_theme(path, source).map_err(|error| config_error(path, error))?;
            invalid_warnings(path, warnings)
        }
        ConfigFileKind::Keybinding => parse_keybindings(path, source)
            .map(|_| ())
            .map_err(keybinding_error),
        ConfigFileKind::Profile => parse_profile(path, source)
            .map(|_| ())
            .map_err(profile_error),
    }
}

fn invalid_warnings(path: &Path, warnings: Vec<String>) -> Result<(), MigrationError> {
    if warnings.is_empty() {
        Ok(())
    } else {
        Err(MigrationError::Invalid {
            path: path.display().to_string(),
            details: warnings.join("; "),
        })
    }
}

fn parse_error(error: ConfigParseDiagnostic) -> MigrationError {
    let error: ConfigError = error.into();
    MigrationError::Parse {
        detail: error.to_string(),
    }
}

fn config_error(path: &Path, error: ConfigError) -> MigrationError {
    match error {
        ConfigError::Parse { .. } => MigrationError::Parse {
            detail: error.to_string(),
        },
        ConfigError::NotFound { .. } | ConfigError::Validation { .. } => MigrationError::Invalid {
            path: path.display().to_string(),
            details: error.to_string(),
        },
    }
}

fn keybinding_error(error: KeybindingParseError) -> MigrationError {
    match error {
        KeybindingParseError::Syntax(error) => parse_error(error),
        KeybindingParseError::Invalid { path, diagnostics } => MigrationError::Invalid {
            path,
            details: diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message())
                .collect::<Vec<_>>()
                .join("; "),
        },
    }
}

fn profile_error(error: ProfileError) -> MigrationError {
    match error {
        ProfileError::Syntax(error) => parse_error(error),
        ProfileError::Invalid { path, diagnostics } => MigrationError::Invalid {
            path,
            details: diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message())
                .collect::<Vec<_>>()
                .join("; "),
        },
    }
}

fn version_error(path: &Path, detail: impl Into<String>) -> MigrationError {
    MigrationError::Version {
        path: path.display().to_string(),
        detail: detail.into(),
    }
}

fn schema_for(schemas: &[Schema], version: u32) -> Result<Schema, MigrationError> {
    schemas
        .iter()
        .copied()
        .find(|schema| schema.version == version)
        .ok_or(MigrationError::MissingSchema { version })
}

fn check_registry(schemas: &[Schema]) -> Result<(), MigrationError> {
    check_registry_for(schemas, SCHEMA_VERSION)
}

fn check_registry_for(schemas: &[Schema], current: u32) -> Result<(), MigrationError> {
    for version in 1..=current {
        let schema = schema_for(schemas, version)?;
        if version < current && schema.migrate_to_next.is_none() {
            return Err(MigrationError::MissingStep {
                from: version,
                to: version + 1,
            });
        }
    }
    Ok(())
}
