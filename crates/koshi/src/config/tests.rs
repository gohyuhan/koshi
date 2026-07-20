//! Tests for config file loading — name validation for the name-selected
//! files (profiles, themes) and the per-file readers that take an explicit
//! path.

use std::path::PathBuf;

use koshi_config::types::RgbColor;

use super::*;

#[test]
fn a_plain_name_is_accepted() {
    assert!(is_plain_file_name("dev"));
    assert!(is_plain_file_name("work.2"));
    assert!(is_plain_file_name("my-profile"));
    assert!(is_plain_file_name("midnight"));
}

#[test]
fn a_path_traversing_or_absolute_name_is_rejected() {
    // Each of these would join to a `.kdl` outside `profile/` or `themes/`.
    assert!(!is_plain_file_name("../secret"));
    assert!(!is_plain_file_name("a/b"));
    assert!(!is_plain_file_name("/etc/passwd"));
    assert!(!is_plain_file_name(".."));
    assert!(!is_plain_file_name("."));
    assert!(!is_plain_file_name(""));
}

#[test]
fn a_nested_or_trailing_separator_name_is_rejected() {
    // `foo/` would read `profile/foo/.kdl` — a nested file, not the flat
    // `profile/<name>.kdl` the rule requires; `foo/..` walks back out.
    assert!(!is_plain_file_name("foo/"));
    assert!(!is_plain_file_name("foo/.."));
}

#[test]
fn a_leading_or_embedded_dot_name_stays_plain() {
    // Only the exact `.` and `..` components are rejected; a leading dot or a
    // double dot inside a longer name is an ordinary flat file name.
    assert!(is_plain_file_name(".hidden"));
    assert!(is_plain_file_name("a..b"));
    assert!(is_plain_file_name("..config"));
    assert!(is_plain_file_name("config.."));
}

#[test]
fn a_backslash_in_a_name_follows_the_platform_separator() {
    // A backslash is a path separator on Windows (so `a\b` names a nested
    // file and is rejected) but an ordinary character on Unix (so `a\b` is a
    // single flat file name and stays plain).
    #[cfg(windows)]
    assert!(!is_plain_file_name("a\\b"));
    #[cfg(not(windows))]
    assert!(is_plain_file_name("a\\b"));
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

// --- load_theme: selecting a `themes/<name>.kdl` by name ---

/// Writes `source` to `themes/<name>.kdl` under `dir`, creating the theme
/// directory, and returns the file's path.
fn write_theme(dir: &Path, name: &str, source: &str) -> PathBuf {
    let themes = dir.join("themes");
    std::fs::create_dir_all(&themes).expect("create themes dir");
    let path = themes.join(format!("{name}.kdl"));
    std::fs::write(&path, source).expect("write");
    path
}

#[test]
fn a_selected_theme_is_read_from_the_themes_directory_and_named_after_its_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    write_theme(
        dir.path(),
        "midnight",
        "colors {\n    accent \"#f5c2ff\"\n}\n",
    );
    let mut warnings = Vec::new();
    let layer = load_theme(dir.path(), "midnight", &mut warnings).expect("theme loads");
    assert_eq!(layer.name, Some("midnight".to_string()));
    assert_eq!(
        layer.colors.expect("colors set").accent,
        Some(RgbColor {
            r: 0xf5,
            g: 0xc2,
            b: 0xff
        })
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn selecting_the_default_theme_by_name_keeps_the_built_in_colors_silently() {
    // `default` is the built-in theme, so it is never looked up on disk — and
    // asking for it is a normal choice, not a problem to warn about.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(load_theme(dir.path(), DEFAULT_THEME, &mut warnings), None);
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn a_default_named_theme_file_is_ignored_in_favor_of_the_built_in_theme() {
    // Even with a `themes/default.kdl` on disk, the reserved name means the
    // built-in colors: the file is never read.
    let dir = tempfile::tempdir().expect("temp dir");
    write_theme(
        dir.path(),
        DEFAULT_THEME,
        "colors {\n    accent \"#ff0000\"\n}\n",
    );
    let mut warnings = Vec::new();
    assert_eq!(load_theme(dir.path(), DEFAULT_THEME, &mut warnings), None);
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn a_theme_with_no_file_falls_back_to_the_default_with_a_warning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(load_theme(dir.path(), "missing", &mut warnings), None);
    assert_eq!(
        warnings,
        vec![format!(
            "theme `missing` not found at {}; using the default theme",
            dir.path().join("themes").join("missing.kdl").display()
        )]
    );
}

#[test]
fn a_path_traversing_theme_name_is_rejected_before_any_file_is_read() {
    // `theme "../../secret"` must not reach a `.kdl` outside `themes/`.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(load_theme(dir.path(), "../../secret", &mut warnings), None);
    assert_eq!(
        warnings,
        vec!["theme name `../../secret` must be a plain name; using the default theme".to_string()]
    );
}

#[test]
fn an_unknown_theme_field_is_kept_as_a_path_prefixed_skip_warning() {
    // A theme file that parses but names an unknown color role applies its
    // other fields and records the skip, prefixed with the file it came from.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write_theme(
        dir.path(),
        "midnight",
        "colors {\n    foreground \"#ffffff\"\n}\n",
    );
    let mut warnings = Vec::new();
    assert!(load_theme(dir.path(), "midnight", &mut warnings).is_some());
    assert_eq!(
        warnings,
        vec![format!(
            "{}: ignored unknown `colors.foreground`",
            path.display()
        )]
    );
}

#[test]
fn a_theme_file_with_an_unsupported_version_falls_back_to_the_default_with_a_warning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write_theme(dir.path(), "midnight", "version 999\n");
    let mut warnings = Vec::new();
    assert_eq!(load_theme(dir.path(), "midnight", &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "theme `midnight` not applied ({}): ",
            path.display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using the default theme"),
        "unexpected warning: {}",
        warnings[0]
    );
}

#[test]
fn an_unreadable_theme_file_reports_the_cause_and_the_fallback_in_one_line() {
    // A directory named `midnight.kdl` exists but reads as no string on any
    // platform, so the read fails with something other than `NotFound` and the
    // built-in theme stands. One warning carries the path, the OS reason, and
    // what koshi used instead.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("themes").join("midnight.kdl");
    std::fs::create_dir_all(&path).expect("create dir in place of the file");
    let mut warnings = Vec::new();
    assert_eq!(load_theme(dir.path(), "midnight", &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "theme `midnight` could not be read ({}): ",
            path.display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using the default theme"),
        "unexpected warning: {}",
        warnings[0]
    );
}

#[test]
fn a_missing_theme_is_reported_as_missing_not_as_unreadable() {
    // The absent case and the unreadable case are told apart by the error kind
    // off a single read, so each warning names the real cause: a theme that was
    // never there says "not found", never "could not be read".
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(dir.path().join("themes")).expect("create themes dir");
    let mut warnings = Vec::new();
    assert_eq!(load_theme(dir.path(), "midnight", &mut warnings), None);
    assert_eq!(
        warnings,
        vec![format!(
            "theme `midnight` not found at {}; using the default theme",
            dir.path().join("themes").join("midnight.kdl").display()
        )]
    );
}

#[test]
fn every_theme_failure_says_which_theme_stands_instead() {
    // One assertion over all four failure paths: whatever went wrong, the last
    // thing the user reads is what koshi actually drew with.
    let dir = tempfile::tempdir().expect("temp dir");
    write_theme(dir.path(), "broken", "version 999\n");
    let unreadable = dir.path().join("themes").join("unreadable.kdl");
    std::fs::create_dir_all(&unreadable).expect("create dir in place of the file");

    for name in ["../../secret", "missing", "unreadable", "broken"] {
        let mut warnings = Vec::new();
        assert_eq!(load_theme(dir.path(), name, &mut warnings), None);
        let last = warnings.last().expect("a warning per failure");
        assert!(
            last.ends_with("; using the default theme"),
            "`{name}` failed without naming the fallback: {last}"
        );
    }
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
    let path = Path::new("themes/midnight.kdl");
    let mut warnings = Vec::new();
    push_field_warnings(path, &[], &mut warnings);
    assert_eq!(warnings, Vec::<String>::new());
}
