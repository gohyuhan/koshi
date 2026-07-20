//! Tests for config file loading — profile-name validation and the
//! per-file readers that take an explicit path.

use super::*;

#[test]
fn a_plain_profile_name_is_accepted() {
    assert!(is_plain_profile_name("dev"));
    assert!(is_plain_profile_name("work.2"));
    assert!(is_plain_profile_name("my-profile"));
}

#[test]
fn a_path_traversing_or_absolute_profile_name_is_rejected() {
    // Each of these would join to a `.kdl` outside `profile/`.
    assert!(!is_plain_profile_name("../secret"));
    assert!(!is_plain_profile_name("a/b"));
    assert!(!is_plain_profile_name("/etc/passwd"));
    assert!(!is_plain_profile_name(".."));
    assert!(!is_plain_profile_name("."));
    assert!(!is_plain_profile_name(""));
}

#[test]
fn a_nested_or_trailing_separator_profile_name_is_rejected() {
    // `foo/` would read `profile/foo/.kdl` — a nested file, not the flat
    // `profile/<name>.kdl` the rule requires; `foo/..` walks back out.
    assert!(!is_plain_profile_name("foo/"));
    assert!(!is_plain_profile_name("foo/.."));
}

#[test]
fn a_leading_or_embedded_dot_name_stays_plain() {
    // Only the exact `.` and `..` components are rejected; a leading dot or a
    // double dot inside a longer name is an ordinary flat file name.
    assert!(is_plain_profile_name(".hidden"));
    assert!(is_plain_profile_name("a..b"));
    assert!(is_plain_profile_name("..config"));
    assert!(is_plain_profile_name("config.."));
}

#[test]
fn a_backslash_in_a_profile_name_follows_the_platform_separator() {
    // A backslash is a path separator on Windows (so `a\b` names a nested
    // file and is rejected) but an ordinary character on Unix (so `a\b` is a
    // single flat file name and stays plain).
    #[cfg(windows)]
    assert!(!is_plain_profile_name("a\\b"));
    #[cfg(not(windows))]
    assert!(is_plain_profile_name("a\\b"));
}

// --- read: absent, present, and unreadable files ---

#[test]
fn reading_an_absent_file_is_none_without_a_warning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(read(&dir.path().join("missing.kdl"), &mut warnings), None);
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn reading_a_present_file_returns_its_exact_text() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("present.kdl");
    std::fs::write(&path, "version 1\n").expect("write");
    let mut warnings = Vec::new();
    assert_eq!(read(&path, &mut warnings), Some("version 1\n".to_string()));
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn reading_a_directory_as_a_file_warns_and_is_none() {
    // A path that exists but is a directory is readable-as-a-string nowhere,
    // so `read` takes its error arm on every platform.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(read(dir.path(), &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "could not read config file {}: ",
            dir.path().display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
}

// --- load_app: clean, field-warning-free, and hard-error files ---

#[test]
fn loading_a_clean_app_file_returns_a_layer_without_warnings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("koshi.kdl");
    std::fs::write(&path, "version 1\n").expect("write");
    let mut warnings = Vec::new();
    assert!(load_app(&path, &mut warnings).is_some());
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn loading_an_absent_app_file_is_none_without_a_warning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(load_app(&dir.path().join("koshi.kdl"), &mut warnings), None);
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn an_app_file_with_an_unsupported_version_drops_to_defaults_with_a_warning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("koshi.kdl");
    std::fs::write(&path, "version 999\n").expect("write");
    let mut warnings = Vec::new();
    assert_eq!(load_app(&path, &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!("koshi.kdl not applied ({}): ", path.display())),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using defaults"),
        "unexpected warning: {}",
        warnings[0]
    );
}

// --- load_theme: clean, field-warning, and hard-error files ---

#[test]
fn loading_a_clean_theme_file_returns_a_layer_without_warnings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("theme.kdl");
    std::fs::write(&path, "name \"midnight\"\n").expect("write");
    let mut warnings = Vec::new();
    assert!(load_theme(&path, &mut warnings).is_some());
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn an_unknown_theme_field_is_kept_as_a_path_prefixed_skip_warning() {
    // A theme file that parses but names an unknown color role applies its
    // other fields and records the skip, prefixed with the file it came from.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("theme.kdl");
    std::fs::write(&path, "colors {\n    foreground \"#ffffff\"\n}\n").expect("write");
    let mut warnings = Vec::new();
    assert!(load_theme(&path, &mut warnings).is_some());
    assert_eq!(
        warnings,
        vec![format!(
            "{}: ignored unknown `colors.foreground`",
            path.display()
        )]
    );
}

#[test]
fn a_theme_file_with_an_unsupported_version_drops_to_defaults_with_a_warning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("theme.kdl");
    std::fs::write(&path, "version 999\n").expect("write");
    let mut warnings = Vec::new();
    assert_eq!(load_theme(&path, &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!("theme.kdl not applied ({}): ", path.display())),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using defaults"),
        "unexpected warning: {}",
        warnings[0]
    );
}

// --- load_keybindings: valid and unparseable files ---

#[test]
fn loading_a_valid_keybinding_file_returns_a_layer_without_warnings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("keybinding.kdl");
    std::fs::write(
        &path,
        "mode \"normal\" {\n    bind \"<C-y>\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    let mut warnings = Vec::new();
    assert!(load_keybindings(&path, &mut warnings).is_some());
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn loading_an_absent_keybinding_file_is_none_without_a_warning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(
        load_keybindings(&dir.path().join("keybinding.kdl"), &mut warnings),
        None
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn an_unparseable_keybinding_file_drops_the_whole_file_with_a_warning() {
    // `keybinding.kdl` is all-or-nothing: any parse error drops the file.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("keybinding.kdl");
    std::fs::write(
        &path,
        "mode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    let mut warnings = Vec::new();
    assert_eq!(load_keybindings(&path, &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "keybinding.kdl not applied ({}): ",
            path.display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using defaults"),
        "unexpected warning: {}",
        warnings[0]
    );
}

// --- push_field_warnings: file-prefixed skip lines ---

#[test]
fn push_field_warnings_prefixes_each_skip_with_the_file_path() {
    let path = Path::new("some/koshi.kdl");
    let mut warnings = vec!["earlier".to_string()];
    push_field_warnings(
        path,
        &["first skip".to_string(), "second skip".to_string()],
        &mut warnings,
    );
    assert_eq!(
        warnings,
        vec![
            "earlier".to_string(),
            format!("{}: first skip", path.display()),
            format!("{}: second skip", path.display()),
        ]
    );
}

#[test]
fn push_field_warnings_adds_nothing_for_an_empty_skip_list() {
    let path = Path::new("theme.kdl");
    let mut warnings = Vec::new();
    push_field_warnings(path, &[], &mut warnings);
    assert_eq!(warnings, Vec::<String>::new());
}
