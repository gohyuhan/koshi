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

/// The `version` reason `validate_config` gives for `source`.
fn version_reason(source: &str) -> String {
    match validate_config(ConfigFileKind::App, Path::new("koshi.kdl"), source).unwrap_err() {
        MigrationError::Version { path, detail } => {
            assert_eq!(path, "koshi.kdl");
            detail
        }
        other => panic!("expected a version error, got {other:?}"),
    }
}

#[test]
fn a_second_version_declaration_is_rejected() {
    assert_eq!(
        version_reason("version 1\nversion 1\n"),
        "`version` is declared more than once"
    );
}

#[test]
fn a_version_with_a_child_block_is_rejected() {
    assert_eq!(
        version_reason("version 1 {\n  extra 2\n}\n"),
        "`version` takes no children"
    );
}

#[test]
fn a_version_with_no_argument_is_rejected() {
    assert_eq!(
        version_reason("version\n"),
        "`version` takes exactly one integer argument"
    );
}

#[test]
fn a_version_with_two_arguments_is_rejected() {
    assert_eq!(
        version_reason("version 1 2\n"),
        "`version` takes exactly one integer argument"
    );
}

#[test]
fn a_version_given_as_a_property_is_rejected() {
    assert_eq!(
        version_reason("version schema=1\n"),
        "`version` takes an argument, not a property"
    );
}

#[test]
fn a_non_integer_version_is_rejected() {
    assert_eq!(
        version_reason("version \"1\"\n"),
        "`version` must be an integer"
    );
}

#[test]
fn a_negative_version_is_rejected() {
    assert_eq!(
        version_reason("version -1\n"),
        "`version` must be between 1 and 4294967295"
    );
}

#[test]
fn a_version_above_the_u32_ceiling_is_rejected() {
    assert_eq!(
        version_reason("version 4294967296\n"),
        "`version` must be between 1 and 4294967295"
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
fn a_registry_missing_a_supported_version_is_refused_before_any_work() {
    let schemas = [Schema {
        version: 2,
        validate: valid_any,
        migrate_to_next: None,
    }];

    let error = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 2\n",
        &schemas,
        2,
    )
    .unwrap_err();

    assert_eq!(error, MigrationError::MissingSchema { version: 1 });
}

#[test]
fn a_step_landing_on_the_wrong_version_stops_the_chain() {
    // `migrate_two` rewrites `version 2` to `version 3`, so running it on the
    // version 1 file leaves the version untouched at 1, not the required 2.
    let schemas = [
        Schema {
            version: 1,
            validate: valid_any,
            migrate_to_next: Some(migrate_two),
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
        MigrationError::Version {
            path: "koshi.kdl".to_string(),
            detail: "migration from version 1 produced version 1, expected 2".to_string(),
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
