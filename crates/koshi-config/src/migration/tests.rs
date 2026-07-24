//! Tests for ordered config validation and migration.

use std::path::Path;

use super::*;

fn valid_any(_kind: ConfigFileKind, path: &Path, source: &str) -> Result<(), MigrationError> {
    read_version(path, source).map(|_| ())
}

fn migrate_one(_path: &Path, source: &str) -> Result<String, MigrationError> {
    Ok(source.replacen("version 1", "version 2", 1) + "step-one #true\n")
}

fn migrate_two(_path: &Path, source: &str) -> Result<String, MigrationError> {
    Ok(source.replacen("version 2", "version 3", 1) + "step-two #true\n")
}

#[test]
fn current_valid_file_stays_byte_for_byte_unchanged() {
    let source = "version 1\ncolors { accent \"#ffffff\" }\n";

    let result =
        migrate_config(ConfigFileKind::Theme, Path::new("themes/plain.kdl"), source).unwrap();

    assert_eq!(result.from, 1);
    assert_eq!(result.to, 1);
    assert!(!result.changed);
    assert_eq!(result.source, source);
}

#[test]
fn production_registry_covers_every_supported_version() {
    check_registry(SCHEMAS).unwrap();
}

#[test]
fn missing_version_is_rejected() {
    let error = validate_config(
        ConfigFileKind::Theme,
        Path::new("themes/plain.kdl"),
        "colors {}\n",
    )
    .unwrap_err();

    assert_eq!(
        error,
        MigrationError::Version {
            path: "themes/plain.kdl".to_string(),
            detail: "file must declare `version`".to_string(),
        }
    );
}

#[test]
fn version_zero_is_rejected() {
    let error =
        validate_config(ConfigFileKind::App, Path::new("koshi.kdl"), "version 0\n").unwrap_err();

    assert_eq!(
        error,
        MigrationError::Version {
            path: "koshi.kdl".to_string(),
            detail: "`version` must be at least 1".to_string(),
        }
    );
}

#[test]
fn bad_kdl_is_rejected_before_migration() {
    let error = migrate_config(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\npane {",
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "config parse error in koshi.kdl: No closing '}' for child block"
    );
}

#[test]
fn field_partial_warning_is_a_validation_error_for_migration() {
    let error = validate_config(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\npane { min-col 2 }\n",
    )
    .unwrap_err();

    assert_eq!(
        error,
        MigrationError::Invalid {
            path: "koshi.kdl".to_string(),
            details: "ignored unknown key `pane.min-col`; did you mean `pane.min-cols`?"
                .to_string(),
        }
    );
}

#[test]
fn migration_runs_every_adjacent_step_in_order() {
    let schemas = [
        Schema {
            version: 1,
            validate: valid_any,
            migrate_to_next: Some(migrate_one),
        },
        Schema {
            version: 2,
            validate: valid_any,
            migrate_to_next: Some(migrate_two),
        },
        Schema {
            version: 3,
            validate: valid_any,
            migrate_to_next: None,
        },
    ];

    let result = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\n",
        &schemas,
        3,
    )
    .unwrap();

    assert_eq!(result.from, 1);
    assert_eq!(result.to, 3);
    assert_eq!(result.source, "version 3\nstep-one #true\nstep-two #true\n");
}

#[test]
fn missing_adjacent_step_stops_the_chain() {
    let schemas = [
        Schema {
            version: 1,
            validate: valid_any,
            migrate_to_next: None,
        },
        Schema {
            version: 2,
            validate: valid_any,
            migrate_to_next: None,
        },
    ];

    let error = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\n",
        &schemas,
        2,
    )
    .unwrap_err();

    assert_eq!(error, MigrationError::MissingStep { from: 1, to: 2 });
}

#[test]
fn bad_source_schema_stops_before_migration() {
    fn reject(_kind: ConfigFileKind, path: &Path, _source: &str) -> Result<(), MigrationError> {
        Err(MigrationError::Invalid {
            path: path.display().to_string(),
            details: "bad old field".to_string(),
        })
    }
    let schemas = [
        Schema {
            version: 1,
            validate: reject,
            migrate_to_next: Some(migrate_one),
        },
        Schema {
            version: 2,
            validate: valid_any,
            migrate_to_next: None,
        },
    ];

    let error = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\n",
        &schemas,
        2,
    )
    .unwrap_err();

    assert_eq!(
        error,
        MigrationError::Invalid {
            path: "koshi.kdl".to_string(),
            details: "bad old field".to_string(),
        }
    );
}

#[test]
fn bad_migrated_schema_stops_the_chain() {
    fn validate_step(
        _kind: ConfigFileKind,
        path: &Path,
        source: &str,
    ) -> Result<(), MigrationError> {
        let version = read_version(path, source)?;
        if version == 2 && !source.contains("required #true") {
            return Err(MigrationError::Invalid {
                path: path.display().to_string(),
                details: "missing required version 2 field".to_string(),
            });
        }
        Ok(())
    }
    let schemas = [
        Schema {
            version: 1,
            validate: validate_step,
            migrate_to_next: Some(migrate_one),
        },
        Schema {
            version: 2,
            validate: validate_step,
            migrate_to_next: None,
        },
    ];

    let error = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\n",
        &schemas,
        2,
    )
    .unwrap_err();

    assert_eq!(
        error,
        MigrationError::Invalid {
            path: "koshi.kdl".to_string(),
            details: "missing required version 2 field".to_string(),
        }
    );
}

#[test]
fn future_version_is_rejected() {
    let error =
        migrate_config(ConfigFileKind::App, Path::new("koshi.kdl"), "version 2\n").unwrap_err();

    assert_eq!(
        error,
        MigrationError::Version {
            path: "koshi.kdl".to_string(),
            detail: "schema version 2 is newer than this koshi supports (1)".to_string(),
        }
    );
}

#[test]
fn validation_names_a_future_version_against_the_running_schema() {
    let error =
        validate_config(ConfigFileKind::App, Path::new("koshi.kdl"), "version 2\n").unwrap_err();

    assert_eq!(
        error,
        MigrationError::Version {
            path: "koshi.kdl".to_string(),
            detail: "schema version 2 is newer than this koshi supports (1)".to_string(),
        }
    );
}
