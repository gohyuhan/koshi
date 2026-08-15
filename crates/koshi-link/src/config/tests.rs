//! Tests for config file loading — name validation for the name-selected
//! files (profiles, themes) and the per-file readers that take an explicit
//! path.

use std::fs;
use std::path::PathBuf;

use koshi_beta::beta_feature;
use koshi_config::layer::PartialLoggingConfig;
use koshi_config::types::RgbColor;
use koshi_core::log::{LogFormat, LogLevel};
use tempfile::TempDir;

use super::*;

/// Stands in for a real beta-gated entry point: the gate decides whether the
/// body runs, and the two answers differ, so a closed gate is visible.
#[beta_feature(otherwise = 0)]
fn mock_beta_entry_point() -> u32 {
    1
}

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
        "version 1\ncolors {\n    accent \"#f5c2ff\"\n}\n",
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
        "version 1\ncolors {\n    foreground \"#ffffff\"\n}\n",
    );
    let mut warnings = Vec::new();
    assert!(load_theme(dir.path(), "midnight", &mut warnings).is_some());
    assert_eq!(
        warnings,
        vec![format!(
            "{}: ignored unknown key `colors.foreground`; did you mean `colors.ramp-end`?",
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
        "version 1\nmode \"normal\" {\n    bind \"<C-y>\" \"core:new-tab\"\n}\n",
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

/// The startup wiring: the `koshi.kdl` knob reaches the process-wide gate.
/// One test walks both answers, because the gate is one flag and separate
/// tests would race each other over it.
#[test]
fn apply_beta_gate_opens_the_gate_only_when_the_file_asks_for_it() {
    let on = PartialKoshiConfig {
        allow_beta_features: Some(true),
        ..Default::default()
    };
    let off = PartialKoshiConfig {
        allow_beta_features: Some(false),
        ..Default::default()
    };

    apply_beta_gate(Some(on.clone()));
    assert!(koshi_beta::allowed());

    apply_beta_gate(Some(off));
    assert!(!koshi_beta::allowed());

    // No `koshi.kdl` at all closes an open gate.
    apply_beta_gate(Some(on));
    assert!(koshi_beta::allowed());
    apply_beta_gate(None);
    assert!(!koshi_beta::allowed());

    // The whole chain from text on disk: the reader `load_app_layer` uses, onto
    // the gate, into a function carrying the attribute. `load_app_layer` takes
    // its directory from the platform, so the file goes to `load_app` here.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("koshi.kdl");

    fs::write(&path, "version 1\nallow-beta-features #true\n").unwrap();
    let mut warnings = Vec::new();
    apply_beta_gate(load_app(&path, &mut warnings).map(|file| file.layer));
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(mock_beta_entry_point(), 1);

    fs::write(&path, "version 1\nallow-beta-features #false\n").unwrap();
    let mut warnings = Vec::new();
    apply_beta_gate(load_app(&path, &mut warnings).map(|file| file.layer));
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(mock_beta_entry_point(), 0);
}

#[test]
fn logging_params_with_no_config_file_are_the_defaults() {
    let session_id = SessionId::new();
    let params = logging_params(None, session_id);

    assert!(!params.enabled);
    assert_eq!(params.level, LogLevel::Warning);
    assert_eq!(params.format, LogFormat::Pretty);
    assert_eq!(params.session_id, session_id);
}

#[test]
fn logging_params_take_the_level_and_format_the_config_names() {
    let session_id = SessionId::new();
    let app = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            enabled: Some(true),
            level: Some(LogLevel::Info),
            format: Some(LogFormat::Json),
        }),
        ..Default::default()
    };

    let params = logging_params(Some(&app), session_id);

    assert!(params.enabled);
    assert_eq!(params.level, LogLevel::Info);
    assert_eq!(params.format, LogFormat::Json);
    assert_eq!(params.session_id, session_id);
}

// --- Who may reach a session's control socket ---

/// A `koshi.kdl` layer setting `allow-other-users` to `allowed` and naming no
/// shared directory.
fn switch_layer(allowed: bool) -> PartialKoshiConfig {
    PartialKoshiConfig {
        allow_other_users: Some(allowed),
        ..Default::default()
    }
}

/// A `koshi.kdl` layer setting `allow-other-users` to `allowed` and naming
/// `dir` as the shared sessions directory.
fn switch_layer_sharing(allowed: bool, dir: &str) -> PartialKoshiConfig {
    PartialKoshiConfig {
        allow_other_users: Some(allowed),
        shared_sessions_dir: Some(Some(PathBuf::from(dir))),
        ..Default::default()
    }
}

/// The directory a policy shares through, or `None` when the session serves
/// only the user who started it. `OtherUsers` carries a closure, so the
/// directory is what a test compares.
fn shared_dir_of(policy: Option<OtherUsers>) -> Option<PathBuf> {
    policy.map(|policy| policy.shared_dir)
}

#[test]
fn a_fresh_install_serves_only_the_user_who_started_the_session() {
    assert_eq!(shared_dir_of(other_users_policy(None, None)), None);
}

#[test]
fn a_config_turning_the_switch_off_serves_only_that_user() {
    assert_eq!(
        shared_dir_of(other_users_policy(Some(&switch_layer(false)), None)),
        None
    );
}

#[test]
fn a_config_turning_the_switch_on_shares_through_the_machine_wide_directory() {
    assert_eq!(
        shared_dir_of(other_users_policy(Some(&switch_layer(true)), None)),
        koshi_paths::shared_sessions_dir()
    );
}

#[test]
fn a_config_naming_a_shared_directory_shares_through_that_one() {
    assert_eq!(
        shared_dir_of(other_users_policy(
            Some(&switch_layer_sharing(true, "/var/run/koshi")),
            None
        )),
        Some(PathBuf::from("/var/run/koshi"))
    );
}

#[test]
fn naming_a_shared_directory_alone_serves_only_this_user() {
    // The directory says where the sockets would go, never who may reach them.
    assert_eq!(
        shared_dir_of(other_users_policy(
            Some(&switch_layer_sharing(false, "/var/run/koshi")),
            None
        )),
        None
    );
}

#[test]
fn the_flag_shares_a_session_whose_config_says_no() {
    let policy = other_users_policy(
        Some(&switch_layer_sharing(false, "/var/run/koshi")),
        Some(true),
    )
    .expect("the flag turns the switch on");

    assert_eq!(policy.shared_dir, PathBuf::from("/var/run/koshi"));
    // A service unit started under the flag keeps serving whatever the file
    // says afterwards, so the live read answers the same every time.
    assert!((policy.still_on)());
    assert!((policy.still_on)());
}

#[test]
fn a_flag_naming_no_other_users_serves_only_this_user() {
    // `--allow-other-users` sends `Some(true)` or nothing, so no command line
    // spells this today. An explicit answer beats the file either way.
    assert_eq!(
        shared_dir_of(other_users_policy(
            Some(&switch_layer_sharing(true, "/var/run/koshi")),
            Some(false)
        )),
        None
    );
}
