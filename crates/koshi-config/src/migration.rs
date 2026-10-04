//! Config schema validation and ordered migration.
//!
//! Each supported schema version owns one validator and, except the newest
//! version, one migration to the next version. A validator lists the schema
//! problems in one file: an unknown key, a bad value, an invalid binding.
//! [`validate_config`] refuses a file with any problem. [`migrate_config`]
//! carries the problems of the source file through every step, and stops on
//! bad KDL, an unusable version, a missing step, or a step whose result has more
//! problems than the source file.

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

type ListSchemaProblemsFunction =
    fn(ConfigFileKind, &Path, &str) -> Result<Vec<String>, MigrationError>;
type MigrateConfigFunction = fn(&Path, &str) -> Result<String, MigrationError>;

#[derive(Clone, Copy)]
struct ConfigSchema {
    schema_version: u32,
    list_schema_problems: ListSchemaProblemsFunction,
    migrate_to_next_schema: Option<MigrateConfigFunction>,
}

const CONFIG_SCHEMAS: &[ConfigSchema] = &[
    ConfigSchema {
        schema_version: 1,
        list_schema_problems,
        migrate_to_next_schema: Some(migrate_schema_one_to_two),
    },
    ConfigSchema {
        schema_version: 2,
        list_schema_problems,
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
    validate_schema_registry(CONFIG_SCHEMAS, SCHEMA_VERSION)?;
    let schema_version = parse_config_schema_version(config_path, config_source_text)?;
    if schema_version > SCHEMA_VERSION {
        return Err(build_newer_version_error(
            config_path,
            schema_version,
            SCHEMA_VERSION,
        ));
    }
    let schema = find_schema_by_version(CONFIG_SCHEMAS, schema_version)?;
    let schema_problems =
        (schema.list_schema_problems)(config_file_kind, config_path, config_source_text)?;
    if !schema_problems.is_empty() {
        return Err(build_schema_problem_error(config_path, &schema_problems));
    }
    Ok(ValidatedConfig {
        schema_version,
        is_current: schema_version == SCHEMA_VERSION,
    })
}

/// Migrates one config file through every adjacent schema version.
///
/// The schema problems of the source file stay in the migrated text:
/// `version 1` followed by `made-up-key "x"` becomes `version 2` followed by
/// `made-up-key "x"`.
///
/// # Errors
/// Returns [`MigrationError`] before producing output when the KDL cannot be
/// parsed, the version is missing or unusable, a schema or migration step is
/// missing, or a step's result has more schema problems than the source file.
/// The [`MigrationError::Invalid`] detail lists the problems of that result.
pub fn migrate_config(
    config_file_kind: ConfigFileKind,
    config_path: &Path,
    config_source_text: &str,
) -> Result<MigratedConfig, MigrationError> {
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
    validate_schema_registry(config_schemas, current_schema_version)?;
    let source_schema_version = parse_config_schema_version(config_path, config_source_text)?;
    if source_schema_version > current_schema_version {
        return Err(build_newer_version_error(
            config_path,
            source_schema_version,
            current_schema_version,
        ));
    }

    let source_schema = find_schema_by_version(config_schemas, source_schema_version)?;
    let source_problem_count =
        (source_schema.list_schema_problems)(config_file_kind, config_path, config_source_text)?
            .len();
    let mut schema_version = source_schema_version;
    let mut migrated_source = config_source_text.to_string();
    while schema_version < current_schema_version {
        let schema = find_schema_by_version(config_schemas, schema_version)?;
        let next_schema_version = schema_version + 1;
        let migrate_to_next_schema =
            schema
                .migrate_to_next_schema
                .ok_or(MigrationError::MissingStep {
                    from_schema_version: schema_version,
                    to_schema_version: next_schema_version,
                })?;
        migrated_source = migrate_to_next_schema(config_path, &migrated_source)?;
        let declared_schema_version = parse_config_schema_version(config_path, &migrated_source)?;
        if declared_schema_version != next_schema_version {
            return Err(MigrationError::Version {
                config_path: config_path.display().to_string(),
                version_error_detail: format!(
                    "migration from version {schema_version} produced version {declared_schema_version}, expected {next_schema_version}"
                ),
            });
        }
        let next_schema = find_schema_by_version(config_schemas, next_schema_version)?;
        let migrated_problems =
            (next_schema.list_schema_problems)(config_file_kind, config_path, &migrated_source)?;
        if migrated_problems.len() > source_problem_count {
            return Err(build_schema_problem_error(config_path, &migrated_problems));
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

fn parse_config_schema_version(
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
        .filter(|kdl_node| kdl_node.name().value() == "version");
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

/// Lists the schema problems in one config file, one message each.
///
/// # Errors
/// Returns [`MigrationError::Parse`] when the KDL cannot be parsed.
fn list_schema_problems(
    config_file_kind: ConfigFileKind,
    config_path: &Path,
    config_source_text: &str,
) -> Result<Vec<String>, MigrationError> {
    match config_file_kind {
        ConfigFileKind::App => match parse_app_config(config_path, config_source_text) {
            Ok(parsed_app_config) => Ok(parsed_app_config.parse_warnings),
            Err(config_error) => list_config_error_problems(config_error),
        },
        ConfigFileKind::Theme => match parse_theme(config_path, config_source_text) {
            Ok((_, parse_warnings)) => Ok(parse_warnings),
            Err(config_error) => list_config_error_problems(config_error),
        },
        ConfigFileKind::Keybinding => match parse_keybindings(config_path, config_source_text) {
            Ok(_) => Ok(Vec::new()),
            Err(KeybindingParseError::Syntax(parse_diagnostic)) => {
                Err(build_parse_error(parse_diagnostic))
            }
            Err(KeybindingParseError::Invalid { diagnostics, .. }) => Ok(diagnostics
                .iter()
                .map(|diagnostic| diagnostic.get_diagnostic_message().to_string())
                .collect()),
        },
        ConfigFileKind::Profile => match parse_profile(config_path, config_source_text) {
            Ok(_) => Ok(Vec::new()),
            Err(ProfileError::Syntax(parse_diagnostic)) => Err(build_parse_error(parse_diagnostic)),
            Err(ProfileError::Invalid { diagnostics, .. }) => Ok(diagnostics
                .iter()
                .map(|diagnostic| diagnostic.get_diagnostic_message().to_string())
                .collect()),
        },
    }
}

/// Turns a [`ConfigError`] from an app or theme parse into its schema
/// problems: a validation error is one problem, and a parse error is
/// [`MigrationError::Parse`].
fn list_config_error_problems(config_error: ConfigError) -> Result<Vec<String>, MigrationError> {
    match config_error {
        ConfigError::Parse { .. } => Err(MigrationError::Parse {
            parse_error_detail: config_error.to_string(),
        }),
        ConfigError::Validation { .. } => Ok(vec![config_error.to_string()]),
    }
}

/// The [`MigrationError::Invalid`] that names every problem in `schema_problems`,
/// joined by `"; "`.
fn build_schema_problem_error(config_path: &Path, schema_problems: &[String]) -> MigrationError {
    MigrationError::Invalid {
        config_path: config_path.display().to_string(),
        validation_error_detail: schema_problems.join("; "),
    }
}

fn build_parse_error(parse_diagnostic: ConfigParseDiagnostic) -> MigrationError {
    let config_error: ConfigError = parse_diagnostic.into();
    MigrationError::Parse {
        parse_error_detail: config_error.to_string(),
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

/// The [`MigrationError::Version`] for a file that declares `schema_version`
/// when this build reads up to `current_schema_version`: `schema version 3 is
/// newer than this koshi supports (2)`.
fn build_newer_version_error(
    config_path: &Path,
    schema_version: u32,
    current_schema_version: u32,
) -> MigrationError {
    build_version_error(
        config_path,
        format!(
            "schema version {schema_version} is newer than this koshi supports ({current_schema_version})"
        ),
    )
}

fn migrate_schema_one_to_two(
    config_path: &Path,
    config_source_text: &str,
) -> Result<String, MigrationError> {
    let config_document = parse_kdl(config_path, config_source_text).map_err(build_parse_error)?;
    let version_node = config_document
        .nodes()
        .iter()
        .find(|kdl_node| kdl_node.name().value() == "version")
        .ok_or_else(|| build_version_error(config_path, "file must declare `version`"))?;
    let version_entry = version_node.entries().first().ok_or_else(|| {
        build_version_error(config_path, "`version` takes exactly one integer argument")
    })?;
    let version_span = version_entry.span();
    let version_start = version_span.offset();
    let version_end = version_start + version_span.len();
    let mut migrated_source = String::with_capacity(config_source_text.len() + 1);
    migrated_source.push_str(&config_source_text[..version_start]);
    migrated_source.push('2');
    migrated_source.push_str(&config_source_text[version_end..]);
    Ok(migrated_source)
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

fn validate_schema_registry(
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
