//! Tests for local config commands.

use std::fs;

use tempfile::TempDir;

use super::*;

#[test]
fn path_prints_the_given_config_directory() {
    let config_directory = TempDir::new().unwrap();

    let command_output =
        run_config_command_in_directory(&ConfigCommand::Path, config_directory.path()).unwrap();

    assert_eq!(
        command_output,
        format!("{}\n", config_directory.path().display())
    );
}

#[test]
fn explain_reports_file_default_and_meaning() {
    let explanation = explain_config_key("koshi.pane.min-cols").unwrap();

    assert_eq!(
        explanation,
        "koshi.pane.min-cols\nfile: koshi.kdl\ndefault: 2\nSmallest pane width in columns.\n"
    );
}

#[test]
fn explain_answers_for_the_pane_gap() {
    let explanation = explain_config_key("koshi.pane.gap").unwrap();

    assert_eq!(
        explanation,
        "koshi.pane.gap\nfile: koshi.kdl\ndefault: 0\n\
         Blank cells between two panes that meet along a split.\n"
    );
}

/// The beta knob is top-level like `theme`, and `explain` answers for it the
/// same way it answers for every other key the parser accepts.
#[test]
fn explain_answers_for_the_top_level_beta_knob() {
    let explanation = explain_config_key("koshi.allow-beta-features").unwrap();

    assert_eq!(
        explanation,
        "koshi.allow-beta-features\nfile: koshi.kdl\ndefault: #false\n\
         Run features still marked beta.\n"
    );
}

/// The other-users knob is top-level like `theme`, and `explain` answers for it
/// the same way it answers for every other key the parser accepts.
#[test]
fn explain_answers_for_the_top_level_other_users_knob() {
    let explanation = explain_config_key("koshi.allow-other-users").unwrap();

    assert_eq!(
        explanation,
        "koshi.allow-other-users\nfile: koshi.kdl\ndefault: #false\n\
         Let other users of this machine reach your sessions.\n"
    );
}

/// The shared directory knob is top-level like `theme`, and `explain` answers
/// for it the same way it answers for every other key the parser accepts.
#[test]
fn explain_answers_for_the_top_level_shared_sessions_dir_knob() {
    let explanation = explain_config_key("koshi.shared-sessions-dir").unwrap();

    assert_eq!(
        explanation,
        "koshi.shared-sessions-dir\nfile: koshi.kdl\n\
         default: \"/tmp/koshi\", %ProgramData%\\koshi on Windows\n\
         Directory the shared session sockets live in.\n"
    );
}

/// The auto-close knob is top-level like `theme`, and `explain` answers for it
/// the same way it answers for every other key the parser accepts.
#[test]
fn explain_answers_for_the_top_level_auto_close_knob() {
    let explanation = explain_config_key("koshi.auto-close-session").unwrap();

    assert_eq!(
        explanation,
        "koshi.auto-close-session\nfile: koshi.kdl\ndefault: #false\n\
         End the session when its last client leaves.\n"
    );
}

#[test]
fn explain_answers_for_a_theme_color() {
    let explanation = explain_config_key("theme.colors.border-hover").unwrap();

    assert_eq!(
        explanation,
        "theme.colors.border-hover\nfile: themes/<name>.kdl\ndefault: \"#af5fff\"\n\
         Pane border under the pointer.\n"
    );
}

#[test]
fn explain_answers_for_a_keybinding_setting() {
    let explanation = explain_config_key("keybinding.chord-timeout-ms").unwrap();

    assert_eq!(
        explanation,
        "keybinding.chord-timeout-ms\nfile: keybinding.kdl\ndefault: 500\n\
         Wait for the next key in a sequence.\n"
    );
}

#[test]
fn explain_answers_for_the_profile_version() {
    let explanation = explain_config_key("profile.version").unwrap();

    assert_eq!(
        explanation,
        "profile.version\nfile: profile/<name>.kdl\ndefault: 1\n\
         Config schema version.\n"
    );
}

#[test]
fn explain_unknown_key_suggests_the_nearest_key() {
    let config_error = explain_config_key("koshi.pane.min-col").unwrap_err();

    assert_eq!(
        config_error.to_string(),
        "config failed: unknown key `koshi.pane.min-col`; did you mean `koshi.pane.min-cols`?"
    );
}

#[test]
fn check_validates_every_known_file_in_sorted_path_order() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    fs::create_dir(config_directory.path().join("profile")).unwrap();
    fs::write(config_directory.path().join("koshi.kdl"), "version 2\n").unwrap();
    fs::write(
        config_directory.path().join("keybinding.kdl"),
        "version 2\nmode \"normal\" {}\n",
    )
    .unwrap();
    fs::write(
        config_directory.path().join("themes").join("z.kdl"),
        "version 2\ncolors {}\n",
    )
    .unwrap();
    fs::write(
        config_directory.path().join("profile").join("a.kdl"),
        "version 2\ntab { pane }\n",
    )
    .unwrap();
    fs::write(
        config_directory.path().join("themes").join("skip.txt"),
        "not config",
    )
    .unwrap();

    let report_text = check_config_directory(config_directory.path()).unwrap();

    assert_eq!(
        report_text,
        format!(
            "{}: valid (version 2)\n{}: valid (version 2)\n{}: valid (version 2)\n{}: valid (version 2)\n",
            config_directory.path().join("keybinding.kdl").display(),
            config_directory.path().join("koshi.kdl").display(),
            config_directory.path().join("profile").join("a.kdl").display(),
            config_directory.path().join("themes").join("z.kdl").display(),
        )
    );
}

#[test]
fn check_collects_errors_from_all_files() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    fs::write(config_directory.path().join("koshi.kdl"), "pane {}\n").unwrap();
    fs::write(
        config_directory.path().join("themes").join("bad.kdl"),
        "version 2\ncolors { accent \"bad\" }\n",
    )
    .unwrap();

    let config_error = check_config_directory(config_directory.path())
        .unwrap_err()
        .to_string();

    assert_eq!(
        config_error,
        format!(
            "config failed: invalid config version in {}: file must declare `version`\ninvalid config file {}: ignored `colors.accent`: color must be 6 hex digits (#RRGGBB), got 3",
            config_directory.path().join("koshi.kdl").display(),
            config_directory.path().join("themes").join("bad.kdl").display(),
        )
    );
}

#[test]
fn check_rejects_a_config_path_that_is_not_a_regular_file() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::create_dir(&app_config_path).unwrap();

    let config_error = check_config_directory(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file",
            app_config_path.display()
        )
    );
}

#[test]
fn check_rejects_a_kdl_directory_below_a_config_folder() {
    let config_directory = TempDir::new().unwrap();
    let theme_config_path = config_directory.path().join("themes").join("bad.kdl");
    fs::create_dir_all(&theme_config_path).unwrap();

    let config_error = check_config_directory(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file",
            theme_config_path.display()
        )
    );
}

#[test]
fn check_reports_read_and_validation_errors_together() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let theme_config_path = config_directory.path().join("themes").join("bad.kdl");
    fs::create_dir(&app_config_path).unwrap();
    fs::create_dir_all(theme_config_path.parent().unwrap()).unwrap();
    fs::write(&theme_config_path, "colors {}\n").unwrap();

    let config_error = check_config_directory(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file\ninvalid config version in {}: file must declare `version`",
            app_config_path.display(),
            theme_config_path.display()
        )
    );
}

fn migrate_config_for_test(
    _config_file_kind: ConfigFileKind,
    config_path: &Path,
    config_source_text: &str,
) -> Result<MigratedConfig, MigrationError> {
    if config_path.ends_with("bad.kdl") {
        return Err(MigrationError::Invalid {
            config_path: config_path.display().to_string(),
            validation_error_detail: "bad source".to_string(),
        });
    }
    Ok(MigratedConfig {
        source_schema_version: 1,
        target_schema_version: 2,
        migrated_source: config_source_text.to_string() + "migrated #true\n",
        is_changed: true,
    })
}

#[test]
fn migrate_writes_nothing_when_any_source_is_invalid() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let invalid_theme_path = config_directory.path().join("themes").join("bad.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();
    fs::write(&invalid_theme_path, "version 1\n").unwrap();

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        migrate_config_for_test,
        write_atomic,
    )
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration stopped before writing any file:\ninvalid config file {}: bad source",
            invalid_theme_path.display()
        )
    );
    assert_eq!(fs::read_to_string(app_config_path).unwrap(), "version 1\n");
    assert_eq!(
        fs::read_to_string(invalid_theme_path).unwrap(),
        "version 1\n"
    );
}

#[test]
fn migrate_replaces_each_changed_file_after_validation() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        migrate_config_for_test,
        write_atomic,
    )
    .unwrap();

    assert_eq!(
        migration_report,
        format!("{}: migrated version 1 to 2\n", app_config_path.display())
    );
    assert_eq!(
        fs::read_to_string(app_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
}

#[cfg(unix)]
#[test]
fn migrate_updates_a_symlink_target_and_keeps_the_link() {
    use std::os::unix::fs::symlink;

    let config_directory = TempDir::new().unwrap();
    let stored_config_path = config_directory.path().join("stored-koshi.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&stored_config_path, "version 1\n").unwrap();
    symlink(&stored_config_path, &app_config_path).unwrap();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        migrate_config_for_test,
        write_atomic,
    )
    .unwrap();

    assert_eq!(
        migration_report,
        format!("{}: migrated version 1 to 2\n", app_config_path.display())
    );
    assert!(fs::symlink_metadata(&app_config_path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(stored_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
}

#[test]
fn migrate_write_failure_reports_files_already_migrated() {
    let config_directory = TempDir::new().unwrap();
    let keybinding_config_path = config_directory.path().join("keybinding.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&keybinding_config_path, "version 1\n").unwrap();
    fs::write(&app_config_path, "version 1\n").unwrap();
    let mut write_attempt_count = 0;

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        migrate_config_for_test,
        |config_path, serialized_config_bytes| {
            write_attempt_count += 1;
            if write_attempt_count == 2 {
                return Err(StorageError::Io {
                    detail: "injected failure".to_string(),
                });
            }
            write_atomic(config_path, serialized_config_bytes)
        },
    )
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration write failed for {}: storage io error: injected failure\n{} may already contain migrated data; check it before retrying\nfiles already migrated before this failure:\n{}: migrated version 1 to 2",
            app_config_path.display(),
            app_config_path.display(),
            keybinding_config_path.display(),
        )
    );
    assert_eq!(
        fs::read_to_string(keybinding_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
    assert_eq!(fs::read_to_string(app_config_path).unwrap(), "version 1\n");
}

#[test]
fn migrate_write_failure_warns_that_the_failing_file_may_have_changed() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        migrate_config_for_test,
        |config_path, serialized_config_bytes| {
            write_atomic(config_path, serialized_config_bytes)?;
            Err(StorageError::Io {
                detail: "injected fsync failure".to_string(),
            })
        },
    )
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration write failed for {}: storage io error: injected fsync failure\n{} may already contain migrated data; check it before retrying",
            app_config_path.display(),
            app_config_path.display(),
        )
    );
    assert_eq!(
        fs::read_to_string(app_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
}

#[test]
fn check_of_a_directory_holding_no_config_file_says_so_and_names_the_directory() {
    let config_directory = TempDir::new().unwrap();

    let report_text = check_config_directory(config_directory.path()).unwrap();

    assert_eq!(
        report_text,
        format!(
            "no config files found in {}\n",
            config_directory.path().display()
        )
    );
}

#[test]
fn migrate_of_a_directory_holding_no_config_file_says_so_and_writes_nothing() {
    let config_directory = TempDir::new().unwrap();
    let mut written_config_paths: Vec<PathBuf> = Vec::new();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        migrate_config_for_test,
        |config_path, _serialized_config_bytes| {
            written_config_paths.push(config_path.to_path_buf());
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(
        migration_report,
        format!(
            "no config files found in {}\n",
            config_directory.path().display()
        )
    );
    assert_eq!(written_config_paths, Vec::<PathBuf>::new());
}

/// A `themes` entry that is not a `.kdl` file is left out of the scan, so a
/// directory holding only such a file reads as holding no config file.
#[test]
fn a_themes_entry_that_is_not_kdl_is_left_out_of_the_scan() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    fs::write(
        config_directory.path().join("themes").join("notes.md"),
        "not config",
    )
    .unwrap();

    let report_text = check_config_directory(config_directory.path()).unwrap();

    assert_eq!(
        report_text,
        format!(
            "no config files found in {}\n",
            config_directory.path().display()
        )
    );
}

#[test]
fn run_config_command_in_directory_routes_explain_to_the_field_table() {
    let config_directory = TempDir::new().unwrap();

    let command_output = run_config_command_in_directory(
        &ConfigCommand::Explain {
            config_key: "koshi.pane.gap".to_string(),
        },
        config_directory.path(),
    )
    .unwrap();

    assert_eq!(
        command_output,
        "koshi.pane.gap\nfile: koshi.kdl\ndefault: 0\n\
         Blank cells between two panes that meet along a split.\n"
    );
}

#[test]
fn run_config_command_in_directory_routes_check_to_the_directory_scan() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 2\n").unwrap();

    let command_output =
        run_config_command_in_directory(&ConfigCommand::Check, config_directory.path()).unwrap();

    assert_eq!(
        command_output,
        format!("{}: valid (version 2)\n", app_config_path.display())
    );
}

/// `migrate` through the real migration table: version 2 is this build's
/// schema, so the file is reported as current and left byte for byte as it is.
#[test]
fn run_config_command_in_directory_migrate_leaves_a_file_already_on_this_schema_untouched() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 2\n").unwrap();

    let command_output =
        run_config_command_in_directory(&ConfigCommand::Migrate, config_directory.path()).unwrap();

    assert_eq!(
        command_output,
        format!("{}: current (version 2)\n", app_config_path.display())
    );
    assert_eq!(fs::read_to_string(&app_config_path).unwrap(), "version 2\n");
}

#[test]
fn check_sorts_two_theme_files_by_path() {
    let config_directory = TempDir::new().unwrap();
    let themes_directory = config_directory.path().join("themes");
    fs::create_dir(&themes_directory).unwrap();
    fs::write(themes_directory.join("z.kdl"), "version 2\ncolors {}\n").unwrap();
    fs::write(themes_directory.join("a.kdl"), "version 2\ncolors {}\n").unwrap();

    let report_text = check_config_directory(config_directory.path()).unwrap();

    assert_eq!(
        report_text,
        format!(
            "{}: valid (version 2)\n{}: valid (version 2)\n",
            themes_directory.join("a.kdl").display(),
            themes_directory.join("z.kdl").display(),
        )
    );
}

#[test]
fn check_reports_a_themes_path_that_is_not_a_directory() {
    let config_directory = TempDir::new().unwrap();
    let themes_path = config_directory.path().join("themes");
    fs::write(&themes_path, "not a directory").unwrap();
    let io_error = fs::read_dir(&themes_path).unwrap_err();

    let config_error = check_config_directory(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!("config failed: read {}: {io_error}", themes_path.display())
    );
}

#[cfg(unix)]
#[test]
fn check_validates_a_config_symlink_through_its_target() {
    use std::os::unix::fs::symlink;

    let config_directory = TempDir::new().unwrap();
    let stored_config_path = config_directory.path().join("stored-koshi.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&stored_config_path, "version 2\n").unwrap();
    symlink(&stored_config_path, &app_config_path).unwrap();

    let report_text = check_config_directory(config_directory.path()).unwrap();

    assert_eq!(
        report_text,
        format!("{}: valid (version 2)\n", app_config_path.display())
    );
}

#[cfg(unix)]
#[test]
fn check_reports_a_config_symlink_that_points_nowhere() {
    use std::os::unix::fs::symlink;

    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    symlink(
        config_directory.path().join("missing.kdl"),
        &app_config_path,
    )
    .unwrap();
    let io_error = fs::canonicalize(&app_config_path).unwrap_err();

    let config_error = check_config_directory(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: {io_error}",
            app_config_path.display()
        )
    );
}

#[cfg(unix)]
#[test]
fn check_rejects_a_config_symlink_that_points_at_a_directory() {
    use std::os::unix::fs::symlink;

    let config_directory = TempDir::new().unwrap();
    let target_directory = config_directory.path().join("target-dir");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::create_dir(&target_directory).unwrap();
    symlink(&target_directory, &app_config_path).unwrap();

    let config_error = check_config_directory(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file",
            app_config_path.display()
        )
    );
}

#[test]
fn migrate_writes_nothing_when_a_config_directory_cannot_be_read() {
    let config_directory = TempDir::new().unwrap();
    let themes_path = config_directory.path().join("themes");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&themes_path, "not a directory").unwrap();
    fs::write(&app_config_path, "version 1\n").unwrap();
    let io_error = fs::read_dir(&themes_path).unwrap_err();
    let mut written_config_paths: Vec<PathBuf> = Vec::new();

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        migrate_config_for_test,
        |config_path, _serialized_config_bytes| {
            written_config_paths.push(config_path.to_path_buf());
            Ok(())
        },
    )
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration stopped before writing any file:\nread {}: {io_error}",
            themes_path.display()
        )
    );
    assert_eq!(written_config_paths, Vec::<PathBuf>::new());
    assert_eq!(fs::read_to_string(app_config_path).unwrap(), "version 1\n");
}

/// Migrates `koshi.kdl` from version 1 to 2 and reports every other file as
/// already on version 2.
fn migrate_only_the_app_file(
    config_file_kind: ConfigFileKind,
    _config_path: &Path,
    config_source_text: &str,
) -> Result<MigratedConfig, MigrationError> {
    if config_file_kind == ConfigFileKind::App {
        return Ok(MigratedConfig {
            source_schema_version: 1,
            target_schema_version: 2,
            migrated_source: config_source_text.to_string() + "migrated #true\n",
            is_changed: true,
        });
    }
    Ok(MigratedConfig {
        source_schema_version: 2,
        target_schema_version: 2,
        migrated_source: config_source_text.to_string(),
        is_changed: false,
    })
}

#[test]
fn migrate_writes_only_the_files_that_changed() {
    let config_directory = TempDir::new().unwrap();
    let keybinding_config_path = config_directory.path().join("keybinding.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&keybinding_config_path, "version 1\n").unwrap();
    fs::write(&app_config_path, "version 1\n").unwrap();
    let mut written_config_paths: Vec<PathBuf> = Vec::new();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        migrate_only_the_app_file,
        |config_path, serialized_config_bytes| {
            written_config_paths.push(config_path.to_path_buf());
            write_atomic(config_path, serialized_config_bytes)
        },
    )
    .unwrap();

    assert_eq!(
        migration_report,
        format!(
            "{}: current (version 2)\n{}: migrated version 1 to 2\n",
            keybinding_config_path.display(),
            app_config_path.display()
        )
    );
    assert_eq!(written_config_paths, vec![app_config_path.clone()]);
    assert_eq!(
        fs::read_to_string(keybinding_config_path).unwrap(),
        "version 1\n"
    );
    assert_eq!(
        fs::read_to_string(app_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
}

#[test]
fn migrate_write_failure_leaves_an_unchanged_file_out_of_the_already_migrated_list() {
    let config_directory = TempDir::new().unwrap();
    let keybinding_config_path = config_directory.path().join("keybinding.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&keybinding_config_path, "version 1\n").unwrap();
    fs::write(&app_config_path, "version 1\n").unwrap();

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        migrate_only_the_app_file,
        |_config_path, _serialized_config_bytes| {
            Err(StorageError::Io {
                detail: "injected failure".to_string(),
            })
        },
    )
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration write failed for {}: storage io error: injected failure\n{} may already contain migrated data; check it before retrying",
            app_config_path.display(),
            app_config_path.display(),
        )
    );
    assert_eq!(
        fs::read_to_string(keybinding_config_path).unwrap(),
        "version 1\n"
    );
    assert_eq!(fs::read_to_string(app_config_path).unwrap(), "version 1\n");
}
