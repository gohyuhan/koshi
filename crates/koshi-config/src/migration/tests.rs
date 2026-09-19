//! Tests for ordered config validation and migration.

use std::path::Path;

use super::*;

fn validate_any_config(
    _config_file_kind: ConfigFileKind,
    config_file_path: &Path,
    config_source_text: &str,
) -> Result<(), MigrationError> {
    read_schema_version(config_file_path, config_source_text).map(|_| ())
}

fn migrate_to_schema_two(
    _config_file_path: &Path,
    config_source_text: &str,
) -> Result<String, MigrationError> {
    Ok(config_source_text.replacen("version 1", "version 2", 1) + "step-one #true\n")
}

fn migrate_to_schema_three(
    _config_file_path: &Path,
    config_source_text: &str,
) -> Result<String, MigrationError> {
    Ok(config_source_text.replacen("version 2", "version 3", 1) + "step-two #true\n")
}

#[test]
fn current_valid_file_stays_byte_for_byte_unchanged() {
    let config_source_text = "version 2\ncolors { accent \"#ffffff\" }\n";

    let migrated_config = migrate_config(
        ConfigFileKind::Theme,
        Path::new("themes/plain.kdl"),
        config_source_text,
    )
    .unwrap();

    assert_eq!(migrated_config.source_schema_version, 2);
    assert_eq!(migrated_config.target_schema_version, 2);
    assert!(!migrated_config.is_changed);
    assert_eq!(migrated_config.migrated_source, config_source_text);
}

#[test]
fn production_registry_covers_every_supported_version() {
    validate_schema_registry(CONFIG_SCHEMAS).unwrap();
}

#[test]
fn missing_version_is_rejected() {
    let migration_error = validate_config(
        ConfigFileKind::Theme,
        Path::new("themes/plain.kdl"),
        "colors {}\n",
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Version {
            config_path: "themes/plain.kdl".to_string(),
            version_error_detail: "file must declare `version`".to_string(),
        }
    );
}

/// The `version` reason `validate_config` gives for `config_source_text`.
fn get_version_error_detail(config_source_text: &str) -> String {
    match validate_config(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        config_source_text,
    )
    .unwrap_err()
    {
        MigrationError::Version {
            config_path,
            version_error_detail,
        } => {
            assert_eq!(config_path, "koshi.kdl");
            version_error_detail
        }
        other => panic!("expected a version error, got {other:?}"),
    }
}

#[test]
fn a_second_version_declaration_is_rejected() {
    assert_eq!(
        get_version_error_detail("version 1\nversion 1\n"),
        "`version` is declared more than once"
    );
}

#[test]
fn a_version_with_a_child_block_is_rejected() {
    assert_eq!(
        get_version_error_detail("version 1 {\n  extra 2\n}\n"),
        "`version` takes no children"
    );
}

#[test]
fn a_version_with_no_argument_is_rejected() {
    assert_eq!(
        get_version_error_detail("version\n"),
        "`version` takes exactly one integer argument"
    );
}

#[test]
fn a_version_with_two_arguments_is_rejected() {
    assert_eq!(
        get_version_error_detail("version 1 2\n"),
        "`version` takes exactly one integer argument"
    );
}

#[test]
fn a_version_given_as_a_property_is_rejected() {
    assert_eq!(
        get_version_error_detail("version schema=1\n"),
        "`version` takes exactly one integer argument"
    );
}

#[test]
fn a_non_integer_version_is_rejected() {
    assert_eq!(
        get_version_error_detail("version \"1\"\n"),
        "`version` must be an integer from 1 to 4294967295"
    );
}

#[test]
fn a_negative_version_is_rejected() {
    assert_eq!(
        get_version_error_detail("version -1\n"),
        "`version` must be an integer from 1 to 4294967295"
    );
}

#[test]
fn a_version_above_the_u32_ceiling_is_rejected() {
    assert_eq!(
        get_version_error_detail("version 4294967296\n"),
        "`version` must be an integer from 1 to 4294967295"
    );
}

#[test]
fn version_zero_is_rejected() {
    let migration_error =
        validate_config(ConfigFileKind::App, Path::new("koshi.kdl"), "version 0\n").unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Version {
            config_path: "koshi.kdl".to_string(),
            version_error_detail: "config schema version must be at least 1".to_string(),
        }
    );
}

#[test]
fn bad_kdl_is_rejected_before_migration() {
    let migration_error = migrate_config(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\npane {",
    )
    .unwrap_err();

    assert_eq!(
        migration_error.to_string(),
        "config parse error in koshi.kdl: No closing '}' for child block"
    );
}

#[test]
fn field_partial_warning_is_a_validation_error_for_migration() {
    let migration_error = validate_config(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 2\npane { min-col 2 }\n",
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Invalid {
            config_path: "koshi.kdl".to_string(),
            validation_error_detail:
                "ignored unknown key `pane.min-col`; did you mean `pane.min-cols`?".to_string(),
        }
    );
}

#[test]
fn migration_runs_every_adjacent_step_in_order() {
    let config_schemas = [
        ConfigSchema {
            schema_version: 1,
            validate_config_file: validate_any_config,
            migrate_to_next_schema: Some(migrate_to_schema_two),
        },
        ConfigSchema {
            schema_version: 2,
            validate_config_file: validate_any_config,
            migrate_to_next_schema: Some(migrate_to_schema_three),
        },
        ConfigSchema {
            schema_version: 3,
            validate_config_file: validate_any_config,
            migrate_to_next_schema: None,
        },
    ];

    let migrated_config = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\n",
        &config_schemas,
        3,
    )
    .unwrap();

    assert_eq!(migrated_config.source_schema_version, 1);
    assert_eq!(migrated_config.target_schema_version, 3);
    assert!(migrated_config.is_changed);
    assert_eq!(
        migrated_config.migrated_source,
        "version 3\nstep-one #true\nstep-two #true\n"
    );
}

#[test]
fn missing_adjacent_step_stops_the_chain() {
    let config_schemas = [
        ConfigSchema {
            schema_version: 1,
            validate_config_file: validate_any_config,
            migrate_to_next_schema: None,
        },
        ConfigSchema {
            schema_version: 2,
            validate_config_file: validate_any_config,
            migrate_to_next_schema: None,
        },
    ];

    let migration_error = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\n",
        &config_schemas,
        2,
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::MissingStep {
            from_schema_version: 1,
            to_schema_version: 2,
        }
    );
}

#[test]
fn bad_source_schema_stops_before_migration() {
    fn reject(
        _config_file_kind: ConfigFileKind,
        config_file_path: &Path,
        _config_source_text: &str,
    ) -> Result<(), MigrationError> {
        Err(MigrationError::Invalid {
            config_path: config_file_path.display().to_string(),
            validation_error_detail: "bad old field".to_string(),
        })
    }
    let config_schemas = [
        ConfigSchema {
            schema_version: 1,
            validate_config_file: reject,
            migrate_to_next_schema: Some(migrate_to_schema_two),
        },
        ConfigSchema {
            schema_version: 2,
            validate_config_file: validate_any_config,
            migrate_to_next_schema: None,
        },
    ];

    let migration_error = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\n",
        &config_schemas,
        2,
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Invalid {
            config_path: "koshi.kdl".to_string(),
            validation_error_detail: "bad old field".to_string(),
        }
    );
}

#[test]
fn bad_migrated_schema_stops_the_chain() {
    fn validate_step(
        _kind: ConfigFileKind,
        config_file_path: &Path,
        config_source_text: &str,
    ) -> Result<(), MigrationError> {
        let schema_version = read_schema_version(config_file_path, config_source_text)?;
        if schema_version == 2 && !config_source_text.contains("required #true") {
            return Err(MigrationError::Invalid {
                config_path: config_file_path.display().to_string(),
                validation_error_detail: "missing required version 2 field".to_string(),
            });
        }
        Ok(())
    }
    let config_schemas = [
        ConfigSchema {
            schema_version: 1,
            validate_config_file: validate_step,
            migrate_to_next_schema: Some(migrate_to_schema_two),
        },
        ConfigSchema {
            schema_version: 2,
            validate_config_file: validate_step,
            migrate_to_next_schema: None,
        },
    ];

    let migration_error = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\n",
        &config_schemas,
        2,
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Invalid {
            config_path: "koshi.kdl".to_string(),
            validation_error_detail: "missing required version 2 field".to_string(),
        }
    );
}

#[test]
fn a_registry_missing_a_supported_version_is_refused_before_any_work() {
    let config_schemas = [ConfigSchema {
        schema_version: 2,
        validate_config_file: validate_any_config,
        migrate_to_next_schema: None,
    }];

    let migration_error = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 2\n",
        &config_schemas,
        2,
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::MissingSchema { schema_version: 1 }
    );
}

#[test]
fn a_step_landing_on_the_wrong_version_stops_the_chain() {
    // `migrate_to_schema_three` rewrites `version 2` to `version 3`, so running it on the
    // version 1 file leaves the version untouched at 1, not the required 2.
    let config_schemas = [
        ConfigSchema {
            schema_version: 1,
            validate_config_file: validate_any_config,
            migrate_to_next_schema: Some(migrate_to_schema_three),
        },
        ConfigSchema {
            schema_version: 2,
            validate_config_file: validate_any_config,
            migrate_to_next_schema: None,
        },
    ];

    let migration_error = migrate_with_registry(
        ConfigFileKind::App,
        Path::new("koshi.kdl"),
        "version 1\n",
        &config_schemas,
        2,
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Version {
            config_path: "koshi.kdl".to_string(),
            version_error_detail: "migration from version 1 produced version 1, expected 2"
                .to_string(),
        }
    );
}

#[test]
fn newer_version_is_rejected() {
    let migration_error =
        migrate_config(ConfigFileKind::App, Path::new("koshi.kdl"), "version 3\n").unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Version {
            config_path: "koshi.kdl".to_string(),
            version_error_detail: "schema version 3 is newer than this koshi supports (2)"
                .to_string(),
        }
    );
}

#[test]
fn an_empty_file_declares_no_version() {
    assert_eq!(get_version_error_detail(""), "file must declare `version`");
}

#[test]
fn a_valid_app_file_reports_the_version_it_declares() {
    let validated_config =
        validate_config(ConfigFileKind::App, Path::new("koshi.kdl"), "version 2\n").unwrap();

    assert_eq!(
        validated_config,
        ValidatedConfig {
            schema_version: 2,
            is_current: true,
        }
    );
}

#[test]
fn a_valid_keybinding_file_reports_the_version_it_declares() {
    let validated_config = validate_config(
        ConfigFileKind::Keybinding,
        Path::new("keybinding.kdl"),
        "version 2\nmode \"normal\" { bind \"<C-y>\" \"core:new-tab\" }\n",
    )
    .unwrap();

    assert_eq!(
        validated_config,
        ValidatedConfig {
            schema_version: 2,
            is_current: true,
        }
    );
}

#[test]
fn every_keybinding_schema_problem_lands_in_one_invalid_error() {
    let migration_error = validate_config(
        ConfigFileKind::Keybinding,
        Path::new("keybinding.kdl"),
        "version 2\nkeybindings { }\nmode \"normal\" { unbind \"<Tab>\" }\n",
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Invalid {
            config_path: "keybinding.kdl".to_string(),
            validation_error_detail: "unknown key `keybindings`; did you mean `version`?; \
                      unknown key `unbind`; did you mean `bind`?"
                .to_string(),
        }
    );
}

#[test]
fn a_valid_profile_file_reports_the_version_it_declares() {
    let validated_config = validate_config(
        ConfigFileKind::Profile,
        Path::new("profile/dev.kdl"),
        "version 2\ntab { pane }\n",
    )
    .unwrap();

    assert_eq!(
        validated_config,
        ValidatedConfig {
            schema_version: 2,
            is_current: true,
        }
    );
}

#[test]
fn a_profile_schema_problem_becomes_an_invalid_error() {
    let migration_error = validate_config(
        ConfigFileKind::Profile,
        Path::new("profile/dev.kdl"),
        "version 2\n",
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Invalid {
            config_path: "profile/dev.kdl".to_string(),
            validation_error_detail: "profile file must define at least one `tab`".to_string(),
        }
    );
}

#[test]
fn a_theme_warning_is_a_validation_error_for_migration() {
    let migration_error = validate_config(
        ConfigFileKind::Theme,
        Path::new("themes/plain.kdl"),
        "version 2\ncolors { accent \"#ffffff\" }\ncolors { }\n",
    )
    .unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Invalid {
            config_path: "themes/plain.kdl".to_string(),
            validation_error_detail: "ignored duplicate `colors` section".to_string(),
        }
    );
}

#[test]
fn validation_names_a_newer_schema_version_against_the_running_schema() {
    let migration_error =
        validate_config(ConfigFileKind::App, Path::new("koshi.kdl"), "version 3\n").unwrap_err();

    assert_eq!(
        migration_error,
        MigrationError::Version {
            config_path: "koshi.kdl".to_string(),
            version_error_detail: "schema version 3 is newer than this koshi supports (2)"
                .to_string(),
        }
    );
}
