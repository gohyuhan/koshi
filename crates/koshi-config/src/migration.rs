//! Config schema validation and ordered migration.
//!
//! Each supported schema version owns one validator and, except the newest
//! version, one migration to the next version. A validator lists the schema
//! problems in one file: an unknown key, a bad value, an invalid binding.
//! [`validate_config`] refuses a file with any problem.
//! [`validate_parsed_app_config`] gives the same answer for a `koshi.kdl` from
//! what [`parse_app_config`] found, and parses no text. [`migrate_config`]
//! carries the problems of the source file through every step, and stops on
//! bad KDL, an unusable version, a missing step, or a step whose result has more
//! problems than the source file.
//!
//! [`parse_app_config`]: crate::app_config::parse_app_config

use std::fmt::Display;
use std::path::Path;

use kdl::KdlDocument;
use miette::SourceSpan;
use thiserror::Error;

use crate::app_config::{parse_app_config_from_document, AppConfigFile};
use crate::error::{format_problem_at_line_and_column, ConfigError};
use crate::keybinding::parse_keybindings_from_document;
use crate::parser::{parse_kdl, parse_supported_schema_version, ParsedConfigFile};
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
    /// The migrated text with its KDL document, or `None` when the file
    /// already declares this build's newest schema version and no step ran.
    pub migrated_config_file: Option<ParsedConfigFile>,
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

/// Validates one config file against the schema version it declares. A file
/// whose `version` is missing, repeated, malformed, `0`, or newer than this
/// build supports is validated against [`SCHEMA_VERSION`], whose file parser
/// names the version problem in file order with the others.
///
/// # Errors
/// Returns [`MigrationError::Parse`] for bad KDL, and
/// [`MigrationError::Invalid`] with every schema problem, a version problem
/// included: ``invalid config file koshi.kdl: line 1, column 1: file must
/// declare `version`; line 2, column 5: invalid config key
/// `update.auto-check`: expected a boolean (#true or #false)``. Returns
/// [`MigrationError::Version`] for a version problem the file parser does not
/// name, and [`MigrationError::MissingSchema`] or
/// [`MigrationError::MissingStep`] for a registry with a gap.
pub fn validate_config(
    config_file_kind: ConfigFileKind,
    config_path: &Path,
    config_source_text: &str,
) -> Result<ValidatedConfig, MigrationError> {
    validate_schema_registry(CONFIG_SCHEMAS, SCHEMA_VERSION)?;
    let config_document = parse_config_document(config_path, config_source_text)?;
    let schema_version = match parse_schema_version_from_document(&config_document, SCHEMA_VERSION)
    {
        Ok(schema_version) => schema_version,
        Err((version_error_span, version_error_detail)) => {
            let current_schema = find_schema_by_version(CONFIG_SCHEMAS, SCHEMA_VERSION)?;
            let schema_problems = (current_schema.list_schema_problems)(
                config_file_kind,
                config_path,
                config_source_text,
                &config_document,
            );
            if schema_problems.is_empty() {
                return Err(build_version_error(
                    config_path,
                    config_source_text,
                    version_error_span,
                    version_error_detail,
                ));
            }
            return Err(build_schema_problem_error(config_path, &schema_problems));
        }
    };
    let schema = find_schema_by_version(CONFIG_SCHEMAS, schema_version)?;
    let schema_problems = (schema.list_schema_problems)(
        config_file_kind,
        config_path,
        config_source_text,
        &config_document,
    );
    resolve_validated_config(config_path, schema_version, &schema_problems)
}

/// [`validate_config`] for a `koshi.kdl` that [`parse_app_config`] already
/// parsed: `app_config_result` is what it gave for the file at `config_path`.
/// Parses no text.
///
/// Gives what [`validate_config`] gives for [`ConfigFileKind::App`] and the
/// same text. Every schema version in this build lists the problems of a
/// `koshi.kdl` with the parser [`parse_app_config`] runs.
///
/// # Errors
/// Returns [`MigrationError::Parse`] for a [`ConfigError::Parse`], and
/// [`MigrationError::Invalid`] with every problem for a
/// [`ConfigError::Validation`] and for a file that parsed with warnings.
/// Returns [`MigrationError::MissingSchema`] or [`MigrationError::MissingStep`]
/// for a registry with a gap.
///
/// [`parse_app_config`]: crate::app_config::parse_app_config
pub fn validate_parsed_app_config(
    config_path: &Path,
    app_config_result: Result<&AppConfigFile, &ConfigError>,
) -> Result<ValidatedConfig, MigrationError> {
    validate_schema_registry(CONFIG_SCHEMAS, SCHEMA_VERSION)?;
    match app_config_result {
        Ok(app_config_file) => resolve_validated_config(
            config_path,
            app_config_file.schema_version,
            &app_config_file.parse_warnings,
        ),
        Err(config_error @ ConfigError::Parse { .. }) => {
            Err(build_parse_error(config_path, config_error))
        }
        Err(ConfigError::Validation { problem_messages }) => {
            Err(build_schema_problem_error(config_path, problem_messages))
        }
    }
}

/// The [`ValidatedConfig`] of a file at `config_path` that declares
/// `schema_version` and whose schema lists `schema_problems`. `version 1` with
/// no problem gives `schema_version` 1 and `is_current` `false`.
///
/// # Errors
/// Returns the [`MigrationError::Invalid`] that names every problem in
/// `schema_problems` when it holds one or more.
fn resolve_validated_config(
    config_path: &Path,
    schema_version: u32,
    schema_problems: &[String],
) -> Result<ValidatedConfig, MigrationError> {
    if !schema_problems.is_empty() {
        return Err(build_schema_problem_error(config_path, schema_problems));
    }
    Ok(ValidatedConfig {
        schema_version,
        is_current: schema_version == SCHEMA_VERSION,
    })
}

/// Migrates one config file, `source_config_file`, through every adjacent
/// schema version. Reads the KDL document `source_config_file` holds and
/// parses no source text again. Each step's text is parsed once.
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
    source_config_file: &ParsedConfigFile,
) -> Result<MigratedConfig, MigrationError> {
    migrate_with_registry(
        config_file_kind,
        source_config_file,
        CONFIG_SCHEMAS,
        SCHEMA_VERSION,
    )
}

fn migrate_with_registry(
    config_file_kind: ConfigFileKind,
    source_config_file: &ParsedConfigFile,
    config_schemas: &[ConfigSchema],
    current_schema_version: u32,
) -> Result<MigratedConfig, MigrationError> {
    validate_schema_registry(config_schemas, current_schema_version)?;
    let config_path = source_config_file.get_config_path();
    let config_source_text = source_config_file.get_source_text();
    let source_document =
        source_config_file
            .config_document
            .as_ref()
            .map_err(|parse_diagnostic| {
                build_parse_error(config_path, &parse_diagnostic.build_config_error())
            })?;
    let source_schema_version = parse_declared_schema_version(
        config_path,
        config_source_text,
        source_document,
        current_schema_version,
    )?;
    if source_schema_version == current_schema_version {
        return Ok(MigratedConfig {
            source_schema_version,
            target_schema_version: source_schema_version,
            migrated_config_file: None,
        });
    }

    let source_schema = find_schema_by_version(config_schemas, source_schema_version)?;
    let source_problem_count = (source_schema.list_schema_problems)(
        config_file_kind,
        config_path,
        config_source_text,
        source_document,
    )
    .len();
    let mut schema_version = source_schema_version;
    let mut migrated_text_and_document: Option<(String, KdlDocument)> = None;
    while schema_version < current_schema_version {
        let next_schema_version = schema_version + 1;
        let schema = find_schema_by_version(config_schemas, schema_version)
            .expect("validate_schema_registry found every supported version");
        let migrate_to_next_schema = schema
            .migrate_to_next_schema
            .expect("validate_schema_registry found a step below the current version");
        let (step_source_text, step_source_document) = match &migrated_text_and_document {
            Some((migrated_source, migrated_document)) => {
                (migrated_source.as_str(), migrated_document)
            }
            None => (config_source_text, source_document),
        };
        let migrated_source =
            migrate_to_next_schema(config_path, step_source_text, step_source_document)?;
        let (migrated_document, declared_schema_version) =
            parse_versioned_config_document(config_path, &migrated_source, current_schema_version)?;
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
        migrated_text_and_document = Some((migrated_source, migrated_document));
        schema_version = next_schema_version;
    }

    let (migrated_source, migrated_document) = migrated_text_and_document
        .expect("a version below the current version takes at least one step");
    Ok(MigratedConfig {
        source_schema_version,
        target_schema_version: schema_version,
        migrated_config_file: Some(ParsedConfigFile {
            config_path: config_path.to_path_buf(),
            config_source_text: migrated_source,
            config_document: Ok(migrated_document),
        }),
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
    let config_document = parse_config_document(config_path, config_source_text)?;
    let schema_version = parse_declared_schema_version(
        config_path,
        config_source_text,
        &config_document,
        current_schema_version,
    )?;
    Ok((config_document, schema_version))
}

/// The schema version `config_document` declares, from 1 through
/// `current_schema_version`. `config_document` is the KDL document parsed
/// from `config_source_text`, the text of the file at `config_path`.
///
/// # Errors
/// Returns [`MigrationError::Version`] with the line and column of the
/// problem when the document declares no usable version.
fn parse_declared_schema_version(
    config_path: &Path,
    config_source_text: &str,
    config_document: &KdlDocument,
    current_schema_version: u32,
) -> Result<u32, MigrationError> {
    parse_schema_version_from_document(config_document, current_schema_version).map_err(
        |(version_error_span, version_error_detail)| {
            build_version_error(
                config_path,
                config_source_text,
                version_error_span,
                version_error_detail,
            )
        },
    )
}

/// The KDL document parsed from `config_source_text`, the text of the file at
/// `config_path`.
///
/// # Errors
/// Returns [`MigrationError::Parse`] when the text is not valid KDL.
fn parse_config_document(
    config_path: &Path,
    config_source_text: &str,
) -> Result<KdlDocument, MigrationError> {
    parse_kdl(config_path, config_source_text).map_err(|parse_diagnostic| {
        build_parse_error(config_path, &ConfigError::from(parse_diagnostic))
    })
}

/// The [`MigrationError::Parse`] for the file at `config_path` whose text
/// `parse_error` refused: `config parse error in koshi.kdl: line 2, column 6:
/// No closing '}' for child block`.
fn build_parse_error(config_path: &Path, parse_error: &ConfigError) -> MigrationError {
    MigrationError::Parse {
        config_path: config_path.display().to_string(),
        parse_error_detail: parse_error.to_string(),
    }
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
/// app or theme file gives its warnings when it parses, and every problem of
/// its [`ConfigError::Validation`] when it is refused.
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
                Err(config_error) => config_error.list_problem_messages(),
            }
        }
        ConfigFileKind::Theme => {
            match parse_theme_from_document(config_document, config_source_text) {
                Ok((_, parse_warnings)) => parse_warnings,
                Err(config_error) => config_error.list_problem_messages(),
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
