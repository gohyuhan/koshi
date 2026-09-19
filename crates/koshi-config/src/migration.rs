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
use crate::error::{ConfigError, ConfigParseDiagnostic, ConfigVersionDiagnostic};
use crate::keybinding::{parse_keybindings, KeybindingParseError};
use crate::parser::{parse_kdl, parse_version_argument};
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
    pub schema_version: u32,
    /// Whether the file already uses this build's newest schema.
    pub is_current: bool,
}

/// Successful migration of one config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigratedConfig {
    /// Schema version declared before migration.
    pub source_schema_version: u32,
    /// Schema version declared after migration.
    pub target_schema_version: u32,
    /// Migrated KDL text, or the original text when no migration was needed.
    pub migrated_source: String,
    /// Whether at least one migration ran.
    pub is_changed: bool,
}

/// Config validation or migration failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MigrationError {
    /// KDL text could not be parsed.
    #[error("{parse_error_detail}")]
    Parse {
        /// Full parse diagnostic as plain text.
        parse_error_detail: String,
    },
    /// The file did not declare one usable schema version.
    #[error("invalid config version in {config_path}: {version_error_detail}")]
    Version {
        /// File path shown to the user.
        config_path: String,
        /// Plain reason the version cannot be used.
        version_error_detail: String,
    },
    /// The file does not match its declared schema.
    #[error("invalid config file {config_path}: {validation_error_detail}")]
    Invalid {
        /// File path shown to the user.
        config_path: String,
        /// All schema problems joined for terminal output.
        validation_error_detail: String,
    },
    /// Koshi has no schema implementation for a declared version.
    #[error("config schema version {schema_version} has no validator in this koshi build")]
    MissingSchema {
        /// Version with no registered schema.
        schema_version: u32,
    },
    /// Koshi has no migration between two adjacent supported versions.
    #[error(
        "no config migration from version {from_schema_version} to version {to_schema_version}"
    )]
    MissingStep {
        /// Source version.
        from_schema_version: u32,
        /// Required next version.
        to_schema_version: u32,
    },
}

type ValidateConfigFunction = fn(ConfigFileKind, &Path, &str) -> Result<(), MigrationError>;
type MigrateConfigFunction = fn(&Path, &str) -> Result<String, MigrationError>;

#[derive(Clone, Copy)]
struct ConfigSchema {
    schema_version: u32,
    validate_config_file: ValidateConfigFunction,
    migrate_to_next_schema: Option<MigrateConfigFunction>,
}

const CONFIG_SCHEMAS: &[ConfigSchema] = &[
    ConfigSchema {
        schema_version: 1,
        validate_config_file: validate_schema,
        migrate_to_next_schema: Some(reject_schema_one_migration),
    },
    ConfigSchema {
        schema_version: 2,
        validate_config_file: validate_schema,
        migrate_to_next_schema: None,
    },
];

/// Validates one config file against the schema version it declares.
///
/// # Errors
/// Returns [`MigrationError`] for bad KDL, a missing or unusable version,
/// an unknown schema version, or any schema error.
pub fn validate_config(
    config_file_kind: ConfigFileKind,
    config_path: &Path,
    config_source_text: &str,
) -> Result<ValidatedConfig, MigrationError> {
    validate_schema_registry(CONFIG_SCHEMAS)?;
    let schema_version = read_schema_version(config_path, config_source_text)?;
    if schema_version < SCHEMA_VERSION {
        return Err(build_old_schema_version_error(config_path, schema_version));
    }
    if schema_version > SCHEMA_VERSION {
        return Err(MigrationError::Version {
            config_path: config_path.display().to_string(),
            version_error_detail: format!(
                "schema version {schema_version} is newer than this koshi supports ({SCHEMA_VERSION})"
            ),
        });
    }
    let schema = find_schema_by_version(CONFIG_SCHEMAS, schema_version)?;
    (schema.validate_config_file)(config_file_kind, config_path, config_source_text)?;
    Ok(ValidatedConfig {
        schema_version,
        is_current: schema_version == SCHEMA_VERSION,
    })
}

/// Migrates one valid config through every adjacent schema version.
///
/// # Errors
/// Returns [`MigrationError`] before producing output when the source is bad,
/// a schema or migration step is missing, or any migrated result is invalid.
pub fn migrate_config(
    config_file_kind: ConfigFileKind,
    config_path: &Path,
    config_source_text: &str,
) -> Result<MigratedConfig, MigrationError> {
    let source_schema_version = read_schema_version(config_path, config_source_text)?;
    if source_schema_version < SCHEMA_VERSION {
        return Err(build_old_schema_version_error(
            config_path,
            source_schema_version,
        ));
    }
    migrate_with_registry(
        config_file_kind,
        config_path,
        config_source_text,
        CONFIG_SCHEMAS,
        SCHEMA_VERSION,
    )
}

fn migrate_with_registry(
    config_file_kind: ConfigFileKind,
    config_path: &Path,
    config_source_text: &str,
    config_schemas: &[ConfigSchema],
    current_schema_version: u32,
) -> Result<MigratedConfig, MigrationError> {
    validate_schema_registry_for(config_schemas, current_schema_version)?;
    let source_schema_version = read_schema_version(config_path, config_source_text)?;
    if source_schema_version > current_schema_version {
        return Err(MigrationError::Version {
            config_path: config_path.display().to_string(),
            version_error_detail: format!(
                "schema version {source_schema_version} is newer than this koshi supports ({current_schema_version})"
            ),
        });
    }

    let mut schema_version = source_schema_version;
    let mut migrated_source = config_source_text.to_string();
    loop {
        let schema = find_schema_by_version(config_schemas, schema_version)?;
        (schema.validate_config_file)(config_file_kind, config_path, &migrated_source)?;
        if schema_version == current_schema_version {
            break;
        }
        let next_schema_version = schema_version + 1;
        let migrate_to_next_schema =
            schema
                .migrate_to_next_schema
                .ok_or(MigrationError::MissingStep {
                    from_schema_version: schema_version,
                    to_schema_version: next_schema_version,
                })?;
        migrated_source = migrate_to_next_schema(config_path, &migrated_source)?;
        let declared_schema_version = read_schema_version(config_path, &migrated_source)?;
        if declared_schema_version != next_schema_version {
            return Err(MigrationError::Version {
                config_path: config_path.display().to_string(),
                version_error_detail: format!(
                    "migration from version {schema_version} produced version {declared_schema_version}, expected {next_schema_version}"
                ),
            });
        }
        schema_version = next_schema_version;
    }

    Ok(MigratedConfig {
        source_schema_version,
        target_schema_version: schema_version,
        is_changed: source_schema_version != schema_version,
        migrated_source,
    })
}

fn read_schema_version(
    config_path: &Path,
    config_source_text: &str,
) -> Result<u32, MigrationError> {
    let config_document = parse_kdl(config_path, config_source_text).map_err(build_parse_error)?;
    parse_schema_version_from_document(config_path, &config_document)
}

fn parse_schema_version_from_document(
    config_path: &Path,
    config_document: &KdlDocument,
) -> Result<u32, MigrationError> {
    let mut version_nodes = config_document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "version");
    let Some(version_node) = version_nodes.next() else {
        return Err(build_version_error(
            config_path,
            "file must declare `version`",
        ));
    };
    if version_nodes.next().is_some() {
        return Err(build_version_error(
            config_path,
            "`version` is declared more than once",
        ));
    }
    let schema_version =
        parse_version_argument(version_node).map_err(|(_, version_error_detail)| {
            build_version_error(config_path, version_error_detail)
        })?;
    if schema_version == 0 {
        return Err(build_version_error(
            config_path,
            ConfigVersionDiagnostic::TooOld.to_string(),
        ));
    }
    Ok(schema_version)
}

fn validate_schema(
    config_file_kind: ConfigFileKind,
    config_path: &Path,
    config_source_text: &str,
) -> Result<(), MigrationError> {
    match config_file_kind {
        ConfigFileKind::App => {
            let parsed_app_config = parse_app_config(config_path, config_source_text)
                .map_err(|parse_error| build_config_error(config_path, parse_error))?;
            reject_parse_warnings(config_path, parsed_app_config.parse_warnings)
        }
        ConfigFileKind::Theme => {
            let (_, parse_warnings) = parse_theme(config_path, config_source_text)
                .map_err(|parse_error| build_config_error(config_path, parse_error))?;
            reject_parse_warnings(config_path, parse_warnings)
        }
        ConfigFileKind::Keybinding => parse_keybindings(config_path, config_source_text)
            .map(|_| ())
            .map_err(build_keybinding_error),
        ConfigFileKind::Profile => parse_profile(config_path, config_source_text)
            .map(|_| ())
            .map_err(build_profile_error),
    }
}

fn reject_parse_warnings(
    config_path: &Path,
    parse_warnings: Vec<String>,
) -> Result<(), MigrationError> {
    if parse_warnings.is_empty() {
        Ok(())
    } else {
        Err(MigrationError::Invalid {
            config_path: config_path.display().to_string(),
            validation_error_detail: parse_warnings.join("; "),
        })
    }
}

fn build_parse_error(parse_diagnostic: ConfigParseDiagnostic) -> MigrationError {
    let config_error: ConfigError = parse_diagnostic.into();
    MigrationError::Parse {
        parse_error_detail: config_error.to_string(),
    }
}

fn build_config_error(config_path: &Path, config_error: ConfigError) -> MigrationError {
    match config_error {
        ConfigError::Parse { .. } => MigrationError::Parse {
            parse_error_detail: config_error.to_string(),
        },
        ConfigError::Validation { .. } => MigrationError::Invalid {
            config_path: config_path.display().to_string(),
            validation_error_detail: config_error.to_string(),
        },
    }
}

fn build_keybinding_error(keybinding_error: KeybindingParseError) -> MigrationError {
    match keybinding_error {
        KeybindingParseError::Syntax(parse_diagnostic) => build_parse_error(parse_diagnostic),
        KeybindingParseError::Invalid {
            keybinding_path,
            diagnostics,
        } => MigrationError::Invalid {
            config_path: keybinding_path,
            validation_error_detail: diagnostics
                .iter()
                .map(|diagnostic| diagnostic.get_diagnostic_message())
                .collect::<Vec<_>>()
                .join("; "),
        },
    }
}

fn build_profile_error(profile_error: ProfileError) -> MigrationError {
    match profile_error {
        ProfileError::Syntax(parse_diagnostic) => build_parse_error(parse_diagnostic),
        ProfileError::Invalid {
            profile_path,
            diagnostics,
        } => MigrationError::Invalid {
            config_path: profile_path,
            validation_error_detail: diagnostics
                .iter()
                .map(|diagnostic| diagnostic.get_diagnostic_message())
                .collect::<Vec<_>>()
                .join("; "),
        },
    }
}

fn build_version_error(
    config_path: &Path,
    version_error_detail: impl Into<String>,
) -> MigrationError {
    MigrationError::Version {
        config_path: config_path.display().to_string(),
        version_error_detail: version_error_detail.into(),
    }
}

fn build_old_schema_version_error(
    config_path: &Path,
    declared_schema_version: u32,
) -> MigrationError {
    build_version_error(
        config_path,
        format!(
            "schema version {declared_schema_version} is older than this koshi supports ({SCHEMA_VERSION})"
        ),
    )
}

fn reject_schema_one_migration(
    config_path: &Path,
    _config_source_text: &str,
) -> Result<String, MigrationError> {
    Err(build_old_schema_version_error(config_path, 1))
}

fn find_schema_by_version(
    config_schemas: &[ConfigSchema],
    schema_version: u32,
) -> Result<ConfigSchema, MigrationError> {
    config_schemas
        .iter()
        .copied()
        .find(|config_schema| config_schema.schema_version == schema_version)
        .ok_or(MigrationError::MissingSchema { schema_version })
}

fn validate_schema_registry(config_schemas: &[ConfigSchema]) -> Result<(), MigrationError> {
    validate_schema_registry_for(config_schemas, SCHEMA_VERSION)
}

fn validate_schema_registry_for(
    config_schemas: &[ConfigSchema],
    current_schema_version: u32,
) -> Result<(), MigrationError> {
    for schema_version in 1..=current_schema_version {
        let config_schema = find_schema_by_version(config_schemas, schema_version)?;
        if schema_version < current_schema_version && config_schema.migrate_to_next_schema.is_none()
        {
            return Err(MigrationError::MissingStep {
                from_schema_version: schema_version,
                to_schema_version: schema_version + 1,
            });
        }
    }
    Ok(())
}
