//! Tests for local config commands.

use std::fs;

use tempfile::TempDir;

use super::*;

#[test]
fn path_prints_the_given_config_directory() {
    let dir = TempDir::new().unwrap();

    let output = run_in_dir(&ConfigCommand::Path, dir.path()).unwrap();

    assert_eq!(output, format!("{}\n", dir.path().display()));
}

#[test]
fn explain_reports_file_default_and_meaning() {
    let output = explain("koshi.pane.min-cols").unwrap();

    assert_eq!(
        output,
        "koshi.pane.min-cols\nfile: koshi.kdl\ndefault: 2\nSmallest pane width in columns.\n"
    );
}

#[test]
fn explain_unknown_key_suggests_the_nearest_key() {
    let error = explain("koshi.pane.min-col").unwrap_err();

    assert_eq!(
        error.to_string(),
        "config failed: unknown key `koshi.pane.min-col`; did you mean `koshi.pane.min-cols`?"
    );
}

#[test]
fn check_validates_every_known_file_in_sorted_path_order() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("themes")).unwrap();
    fs::create_dir(dir.path().join("profile")).unwrap();
    fs::write(dir.path().join("koshi.kdl"), "version 1\n").unwrap();
    fs::write(
        dir.path().join("keybinding.kdl"),
        "version 1\nmode \"normal\" {}\n",
    )
    .unwrap();
    fs::write(dir.path().join("themes/z.kdl"), "version 1\ncolors {}\n").unwrap();
    fs::write(
        dir.path().join("profile/a.kdl"),
        "version 1\ntab { pane }\n",
    )
    .unwrap();
    fs::write(dir.path().join("themes/skip.txt"), "not config").unwrap();

    let output = check(dir.path()).unwrap();

    assert_eq!(
        output,
        format!(
            "{}: valid (version 1)\n{}: valid (version 1)\n{}: valid (version 1)\n{}: valid (version 1)\n",
            dir.path().join("keybinding.kdl").display(),
            dir.path().join("koshi.kdl").display(),
            dir.path().join("profile/a.kdl").display(),
            dir.path().join("themes/z.kdl").display(),
        )
    );
}

#[test]
fn check_collects_errors_from_all_files() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("themes")).unwrap();
    fs::write(dir.path().join("koshi.kdl"), "pane {}\n").unwrap();
    fs::write(
        dir.path().join("themes/bad.kdl"),
        "version 1\ncolors { accent \"bad\" }\n",
    )
    .unwrap();

    let error = check(dir.path()).unwrap_err().to_string();

    assert_eq!(
        error,
        format!(
            "config failed: invalid config version in {}: file must declare `version`\ninvalid config file {}: ignored `colors.accent`: color must be 6 hex digits (#RRGGBB), got 3",
            dir.path().join("koshi.kdl").display(),
            dir.path().join("themes/bad.kdl").display(),
        )
    );
}

#[test]
fn check_rejects_a_config_path_that_is_not_a_regular_file() {
    let dir = TempDir::new().unwrap();
    let app = dir.path().join("koshi.kdl");
    fs::create_dir(&app).unwrap();

    let error = check(dir.path()).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "config failed: read {}: expected a regular file",
            app.display()
        )
    );
}

#[test]
fn check_rejects_a_kdl_directory_below_a_config_folder() {
    let dir = TempDir::new().unwrap();
    let theme = dir.path().join("themes/bad.kdl");
    fs::create_dir_all(&theme).unwrap();

    let error = check(dir.path()).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "config failed: read {}: expected a regular file",
            theme.display()
        )
    );
}

#[test]
fn check_reports_read_and_validation_errors_together() {
    let dir = TempDir::new().unwrap();
    let app = dir.path().join("koshi.kdl");
    let theme = dir.path().join("themes/bad.kdl");
    fs::create_dir(&app).unwrap();
    fs::create_dir_all(theme.parent().unwrap()).unwrap();
    fs::write(&theme, "colors {}\n").unwrap();

    let error = check(dir.path()).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "config failed: read {}: expected a regular file\ninvalid config version in {}: file must declare `version`",
            app.display(),
            theme.display()
        )
    );
}

fn fake_migrate(
    _kind: ConfigFileKind,
    path: &Path,
    source: &str,
) -> Result<MigratedConfig, MigrationError> {
    if path.ends_with("bad.kdl") {
        return Err(MigrationError::Invalid {
            path: path.display().to_string(),
            details: "bad source".to_string(),
        });
    }
    Ok(MigratedConfig {
        from: 1,
        to: 2,
        source: source.to_string() + "migrated #true\n",
        changed: true,
    })
}

#[test]
fn migrate_writes_nothing_when_any_source_is_invalid() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("themes")).unwrap();
    let app = dir.path().join("koshi.kdl");
    let bad = dir.path().join("themes/bad.kdl");
    fs::write(&app, "version 1\n").unwrap();
    fs::write(&bad, "version 1\n").unwrap();

    let error = migrate_in_dir_with(dir.path(), fake_migrate).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "config failed: migration stopped before writing any file:\ninvalid config file {}: bad source",
            bad.display()
        )
    );
    assert_eq!(fs::read_to_string(app).unwrap(), "version 1\n");
    assert_eq!(fs::read_to_string(bad).unwrap(), "version 1\n");
}

#[test]
fn migrate_replaces_each_changed_file_after_validation() {
    let dir = TempDir::new().unwrap();
    let app = dir.path().join("koshi.kdl");
    fs::write(&app, "version 1\n").unwrap();

    let output = migrate_in_dir_with(dir.path(), fake_migrate).unwrap();

    assert_eq!(
        output,
        format!("{}: migrated version 1 to 2\n", app.display())
    );
    assert_eq!(
        fs::read_to_string(app).unwrap(),
        "version 1\nmigrated #true\n"
    );
}

#[cfg(unix)]
#[test]
fn migrate_updates_a_symlink_target_and_keeps_the_link() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let target = dir.path().join("stored-koshi.kdl");
    let app = dir.path().join("koshi.kdl");
    fs::write(&target, "version 1\n").unwrap();
    symlink(&target, &app).unwrap();

    let output = migrate_in_dir_with(dir.path(), fake_migrate).unwrap();

    assert_eq!(
        output,
        format!("{}: migrated version 1 to 2\n", app.display())
    );
    assert!(fs::symlink_metadata(&app).unwrap().file_type().is_symlink());
    assert_eq!(
        fs::read_to_string(target).unwrap(),
        "version 1\nmigrated #true\n"
    );
}
