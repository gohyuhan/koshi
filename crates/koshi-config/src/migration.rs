//! Config schema validation and ordered migration.
//!
//! Each supported schema version owns one validator and, except the newest
//! version, one migration to the next version. A validator lists the schema
//! problems in one file: an unknown key, a bad value, an invalid binding.
//! [`validate_config`] refuses a file with any problem. [`migrate_config`]
//! carries the problems of the source file through every step, and stops on
//! bad KDL, an unusable version, a missing step, or a step whose result has more
//! problems than the source file.

use std::fmt::Display;
use std::path::Path;

use kdl::KdlDocument;
use miette::SourceSpan;
use thiserror::Error;

use crate::app_config::parse_app_config_from_document;
use crate::error::{format_problem_at_line_and_column, ConfigError};
use crate::keybinding::parse_keybindings_from_document;
use crate::parser::{parse_kdl, parse_supported_schema_version};
use crate::profile::parse_profile_from_document;
use crate::theme::parse_theme_from_document;
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
    #[error("config parse error in {config_path}: {parse_error_detail}")]
    Parse {
        /// File path shown to the user.
        config_path: String,
        /// The [`ConfigError::Parse`] text: `line <L>, column <C>: <message>`.
        parse_error_detail: String,
    },
    /// The file did not declare one usable schema version.
    #[error("invalid config version in {config_path}: {version_error_detail}")]
    Version {
        /// File path shown to the user.
        config_path: String,
        /// The reason the version cannot be used, after the line and column
        /// of the node or value at fault: ``line 2, column 1: `version` is
        /// declared more than once``. A migration step that lands on the wrong
        /// version gives the reason alone.
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

/// Lists the schema problems of one file, given its path, its text, and the
/// KDL document parsed from that text.
type ListSchemaProblemsFunction = fn(ConfigFileKind, &Path, &str, &KdlDocument) -> Vec<String>;
/// Migrates one file to the next schema version, given its path, its text, and
/// the KDL document parsed from that text.
type MigrateConfigFunction = fn(&Path, &str, &KdlDocument) -> Result<String, MigrationError>;

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
    let (config_document, schema_version) =
        parse_versioned_config_document(config_path, config_source_text, SCHEMA_VERSION)?;
    let schema = find_schema_by_version(CONFIG_SCHEMAS, schema_version)?;
    let schema_problems = (schema.list_schema_problems)(
        config_file_kind,
        config_path,
        config_source_text,
        &config_document,
    );
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
    let (source_document, source_schema_version) =
        parse_versioned_config_document(config_path, config_source_text, current_schema_version)?;
    if source_schema_version == current_schema_version {
        return Ok(MigratedConfig {
            source_schema_version,
            target_schema_version: source_schema_version,
            migrated_source: config_source_text.to_string(),
            is_changed: false,
        });
    }

    let source_schema = find_schema_by_version(config_schemas, source_schema_version)?;
    let source_problem_count = (source_schema.list_schema_problems)(
        config_file_kind,
        config_path,
        config_source_text,
        &source_document,
    )
    .len();
    let mut schema_version = source_schema_version;
    let mut migrated_source = config_source_text.to_string();
    let mut migrated_document = source_document;
    while schema_version < current_schema_version {
        let next_schema_version = schema_version + 1;
        let schema = find_schema_by_version(config_schemas, schema_version)
            .expect("validate_schema_registry found every supported version");
        let migrate_to_next_schema = schema
            .migrate_to_next_schema
            .expect("validate_schema_registry found a step below the current version");
        migrated_source =
            migrate_to_next_schema(config_path, &migrated_source, &migrated_document)?;
        let (next_document, declared_schema_version) =
            parse_versioned_config_document(config_path, &migrated_source, current_schema_version)?;
        migrated_document = next_document;
        if declared_schema_version != next_schema_version {
            return Err(MigrationError::Version {
                config_path: config_path.display().to_string(),
                version_error_detail: format!(
                    "migration from version {schema_version} produced version {declared_schema_version}, expected {next_schema_version}"
                ),
            });
        }
        let next_schema = find_schema_by_version(config_schemas, next_schema_version)
            .expect("validate_schema_registry found every supported version");
        let migrated_problems = (next_schema.list_schema_problems)(
            config_file_kind,
            config_path,
            &migrated_source,
            &migrated_document,
        );
        if migrated_problems.len() > source_problem_count {
            return Err(build_schema_problem_error(config_path, &migrated_problems));
        }
        schema_version = next_schema_version;
    }

    Ok(MigratedConfig {
        source_schema_version,
        target_schema_version: schema_version,
        is_changed: true,
        migrated_source,
    })
}

/// The KDL document parsed from `config_source_text` and the schema version
/// it declares, from 1 through `current_schema_version`.
///
/// # Errors
/// Returns [`MigrationError::Parse`] when the text is not valid KDL, and
/// [`MigrationError::Version`] with the line and column of the problem when
/// the document declares no usable version.
fn parse_versioned_config_document(
    config_path: &Path,
    config_source_text: &str,
    current_schema_version: u32,
) -> Result<(KdlDocument, u32), MigrationError> {
    let config_document =
        parse_kdl(config_path, config_source_text).map_err(|parse_diagnostic| {
            MigrationError::Parse {
                config_path: config_path.display().to_string(),
                parse_error_detail: ConfigError::from(parse_diagnostic).to_string(),
            }
        })?;
    let schema_version =
        parse_schema_version_from_document(&config_document, current_schema_version).map_err(
            |(version_error_span, version_error_detail)| {
                build_version_error(
                    config_path,
                    config_source_text,
                    version_error_span,
                    version_error_detail,
                )
            },
        )?;
    Ok((config_document, schema_version))
}

/// The schema version `config_document` declares, from 1 through
/// `current_schema_version`.
///
/// # Errors
/// Returns the span a caret points at and the reason: the span where the
/// first node starts and ``file must declare `version` `` when no `version`
/// node exists; the second `version` node and ``` `version` is declared more
/// than once ```; or the error [`parse_supported_schema_version`] gives.
fn parse_schema_version_from_document(
    config_document: &KdlDocument,
    current_schema_version: u32,
) -> Result<u32, (SourceSpan, String)> {
    let mut version_nodes = config_document
        .nodes()
        .iter()
        .filter(|kdl_node| kdl_node.name().value() == "version");
    let Some(version_node) = version_nodes.next() else {
        return Err((
            config_document.span(),
            "file must declare `version`".to_string(),
        ));
    };
    if let Some(repeated_version_node) = version_nodes.next() {
        return Err((
            repeated_version_node.span(),
            "`version` is declared more than once".to_string(),
        ));
    }
    parse_supported_schema_version(version_node, current_schema_version)
}

/// Lists the schema problems of one config file, one message each, read from
/// `config_document`, the KDL document parsed from `config_source_text`. An
/// app or theme file that fails as a whole gives its one error as the one
/// problem.
fn list_schema_problems(
    config_file_kind: ConfigFileKind,
    config_path: &Path,
    config_source_text: &str,
    config_document: &KdlDocument,
) -> Vec<String> {
    match config_file_kind {
        ConfigFileKind::App => {
            match parse_app_config_from_document(config_document, config_source_text) {
                Ok(parsed_app_config) => parsed_app_config.parse_warnings,
                Err(config_error) => vec![config_error.to_string()],
            }
        }
        ConfigFileKind::Theme => {
            match parse_theme_from_document(config_document, config_source_text) {
                Ok((_, parse_warnings)) => parse_warnings,
                Err(config_error) => vec![config_error.to_string()],
            }
        }
        ConfigFileKind::Keybinding => {
            match parse_keybindings_from_document(config_path, config_source_text, config_document)
            {
                Ok(_) => Vec::new(),
                Err(keybinding_parse_error) => keybinding_parse_error.list_problem_messages(),
            }
        }
        ConfigFileKind::Profile => {
            match parse_profile_from_document(config_path, config_source_text, config_document) {
                Ok(_) => Vec::new(),
                Err(profile_error) => profile_error.list_problem_messages(),
            }
        }
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

/// The [`MigrationError::Version`] for the file at `config_path` whose text
/// `config_source_text` has the problem `version_error_detail` at the start of
/// `version_error_span`: ``line 2, column 1: `version` is declared more than
/// once``.
fn build_version_error(
    config_path: &Path,
    config_source_text: &str,
    version_error_span: SourceSpan,
    version_error_detail: impl Display,
) -> MigrationError {
    MigrationError::Version {
        config_path: config_path.display().to_string(),
        version_error_detail: format_problem_at_line_and_column(
            config_source_text,
            version_error_span,
            version_error_detail,
        ),
    }
}

fn migrate_schema_one_to_two(
    config_path: &Path,
    config_source_text: &str,
    config_document: &KdlDocument,
) -> Result<String, MigrationError> {
    let version_node = config_document
        .nodes()
        .iter()
        .find(|kdl_node| kdl_node.name().value() == "version")
        .ok_or_else(|| {
            build_version_error(
                config_path,
                config_source_text,
                config_document.span(),
                "file must declare `version`",
            )
        })?;
    let version_entry = version_node.entries().first().ok_or_else(|| {
        build_version_error(
            config_path,
            config_source_text,
            version_node.span(),
            "`version` takes exactly one integer argument",
        )
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
