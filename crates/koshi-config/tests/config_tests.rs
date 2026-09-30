//! Guards the `## Full example` block of every page in `config-docs/`. Each
//! block is read straight out of the markdown, fed through its real parser, and
//! checked against the built-in defaults the page says it documents. `koshi.kdl`
//! and the theme file parse with no warnings at all; a misspelled field name is
//! reported as an `ignored ...` warning, not an error.

use std::path::Path;

use koshi_config::app_config::parse_app_config;
use koshi_config::key::Leader;
use koshi_config::keybinding::parse_keybindings;
use koshi_config::layer::{merge_client, merge_server};
use koshi_config::profile::parse_profile;
use koshi_config::theme::parse_theme;
use koshi_config::types::{
    build_default_mode_bindings, ClientConfig, ColorPalette, ServerConfig, DEFAULT_THEME_NAME,
};

/// The KDL text of the fenced block under the `## Full example` heading of
/// `config-docs/<config_doc_file_name>` — the complete example the docs tell a
/// user to copy.
///
/// Every `\r\n` in the page becomes `\n` before the headings and fences are
/// looked for: a checkout that stores the page with Windows line endings reads
/// the same as one that stores it with Unix line endings.
///
/// # Panics
/// Panics when the page cannot be read, carries no `## Full example` heading,
/// or has no closed ```` ```kdl ```` block after that heading.
fn load_full_example(config_doc_file_name: &str) -> String {
    let config_doc_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config-docs")
        .join(config_doc_file_name);
    let config_doc_markdown = std::fs::read_to_string(&config_doc_path)
        .unwrap_or_else(|read_error| {
            panic!("{} is readable: {read_error}", config_doc_path.display())
        })
        .replace("\r\n", "\n");
    let after_heading = config_doc_markdown
        .split_once("\n## Full example\n")
        .unwrap_or_else(|| panic!("{config_doc_file_name} has a `## Full example` heading"))
        .1;
    let inside_fence = after_heading
        .split_once("```kdl\n")
        .unwrap_or_else(|| {
            panic!("{config_doc_file_name} opens a kdl block under `## Full example`")
        })
        .1;
    inside_fence
        .split_once("\n```")
        .unwrap_or_else(|| panic!("{config_doc_file_name} closes its kdl block"))
        .0
        .to_string()
}

#[test]
fn koshi_example_parses_without_warnings() {
    let config_source_text = load_full_example("koshi.md");
    let app_config =
        parse_app_config(Path::new("koshi.kdl"), &config_source_text).expect("koshi.kdl parses");
    assert_eq!(app_config.parse_warnings, Vec::<String>::new());
    // The documented example names the built-in theme.
    assert_eq!(app_config.theme_name, Some(DEFAULT_THEME_NAME.to_string()));
    // Every value it spells out is the built-in default: folding its layer
    // onto the defaults leaves both sides unchanged.
    assert_eq!(
        merge_server(ServerConfig::default(), vec![app_config.layer.clone()]),
        ServerConfig::default()
    );
    assert_eq!(
        merge_client(ClientConfig::default(), vec![app_config.layer]),
        ClientConfig::default()
    );
}

#[test]
fn theme_example_parses_without_warnings() {
    let config_source_text = load_full_example("theme.md");
    let (parsed_theme, parse_warnings) =
        parse_theme(Path::new("themes/default.kdl"), &config_source_text)
            .expect("theme file parses");
    assert_eq!(parse_warnings, Vec::<String>::new());

    // The page documents every color at its default value.
    let parsed_colors = parsed_theme
        .colors
        .expect("the example sets a `colors` block");
    let default_palette = ColorPalette::default();
    for (color_role, parsed_color, default_color) in [
        (
            "ramp-start",
            parsed_colors.ramp_start,
            default_palette.ramp_start,
        ),
        ("ramp-end", parsed_colors.ramp_end, default_palette.ramp_end),
        ("on-ramp", parsed_colors.on_ramp, default_palette.on_ramp),
        (
            "on-ramp-dim",
            parsed_colors.on_ramp_dim,
            default_palette.on_ramp_dim,
        ),
        ("accent", parsed_colors.accent, default_palette.accent),
        (
            "on-accent",
            parsed_colors.on_accent,
            default_palette.on_accent,
        ),
        ("bar-bg", parsed_colors.bar_bg, default_palette.bar_bg),
        (
            "border-focused",
            parsed_colors.border_focused,
            default_palette.border_focused,
        ),
        (
            "border-unfocused",
            parsed_colors.border_unfocused,
            default_palette.border_unfocused,
        ),
        (
            "border-hover",
            parsed_colors.border_hover,
            default_palette.border_hover,
        ),
        (
            "stack-header-fg",
            parsed_colors.stack_header_fg,
            default_palette.stack_header_fg,
        ),
        (
            "stack-header-bg",
            parsed_colors.stack_header_bg,
            default_palette.stack_header_bg,
        ),
        (
            "letterbox",
            parsed_colors.letterbox,
            default_palette.letterbox,
        ),
    ] {
        assert_eq!(
            parsed_color,
            Some(default_color),
            "documented `{color_role}`"
        );
    }
}

#[test]
fn keybinding_example_parses() {
    let config_source_text = load_full_example("keybinding.md");
    let keybinding_layer = parse_keybindings(Path::new("keybinding.kdl"), &config_source_text)
        .expect("keybinding.kdl parses");

    // The page documents the complete built-in keymap: the layer it parses to
    // is the shipped default table, key for key.
    assert_eq!(keybinding_layer.chord_timeout_ms, Some(500));
    assert_eq!(keybinding_layer.which_key_delay_ms, Some(300));
    assert_eq!(keybinding_layer.maximum_chord_depth, Some(4));
    assert_eq!(keybinding_layer.leader, Some(Leader::default()));
    assert_eq!(keybinding_layer.unlock_alternative, None);
    assert_eq!(
        keybinding_layer.mode_bindings_by_name,
        Some(build_default_mode_bindings(Leader::default()))
    );
}

#[test]
fn profile_example_parses() {
    let config_source_text = load_full_example("profile.md");
    let profile_template =
        parse_profile(Path::new("profile/dev.kdl"), &config_source_text).expect("profile parses");

    // Two tabs; the `focus` marker on the second one selects it at open.
    assert_eq!(profile_template.tabs.len(), 2);
    assert_eq!(profile_template.focused_tab_index, 1);
    assert!(!profile_template.is_locked);
    // The editor pane carries `focus` and wins the first tab. The stack tab
    // marks no pane `focus` and falls back to the first visible leaf — the
    // `expanded` stack member, `htop`, at index 1.
    assert_eq!(profile_template.tabs[0].focused_leaf_index, 0);
    assert_eq!(profile_template.tabs[1].focused_leaf_index, 1);
}

/// Every ready-made theme shipped in `themes-example/` parses with no warnings
/// and sets all thirteen color roles.
///
/// The parser skips an unknown role name with a warning. An unset role keeps
/// that part of the chrome at koshi's default color.
#[test]
fn every_shipped_example_theme_is_complete_and_warning_free() {
    let themes_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes-example");
    let mut checked_theme_count = 0;
    for theme_directory_entry in
        std::fs::read_dir(&themes_directory).expect("themes-example directory exists")
    {
        let theme_path = theme_directory_entry
            .expect("readable directory entry")
            .path();
        if theme_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("kdl")
        {
            continue;
        }
        let theme_file_name = theme_path
            .file_name()
            .expect("a file name")
            .to_string_lossy();
        let theme_source_text =
            std::fs::read_to_string(&theme_path).expect("theme file is readable");
        let (parsed_theme, parse_warnings) = parse_theme(&theme_path, &theme_source_text)
            .unwrap_or_else(|parse_error| {
                panic!("{theme_file_name} does not parse: {parse_error}")
            });
        assert_eq!(
            parse_warnings,
            Vec::<String>::new(),
            "{theme_file_name} has warnings"
        );

        let parsed_colors = parsed_theme
            .colors
            .unwrap_or_else(|| panic!("{theme_file_name} has no `colors` block"));
        for (color_role, is_set) in [
            ("ramp-start", parsed_colors.ramp_start.is_some()),
            ("ramp-end", parsed_colors.ramp_end.is_some()),
            ("on-ramp", parsed_colors.on_ramp.is_some()),
            ("on-ramp-dim", parsed_colors.on_ramp_dim.is_some()),
            ("accent", parsed_colors.accent.is_some()),
            ("on-accent", parsed_colors.on_accent.is_some()),
            ("bar-bg", parsed_colors.bar_bg.is_some()),
            ("border-focused", parsed_colors.border_focused.is_some()),
            ("border-unfocused", parsed_colors.border_unfocused.is_some()),
            ("border-hover", parsed_colors.border_hover.is_some()),
            ("stack-header-fg", parsed_colors.stack_header_fg.is_some()),
            ("stack-header-bg", parsed_colors.stack_header_bg.is_some()),
            ("letterbox", parsed_colors.letterbox.is_some()),
        ] {
            assert!(is_set, "{theme_file_name} does not set `{color_role}`");
        }
        checked_theme_count += 1;
    }
    assert!(
        checked_theme_count >= 20,
        "expected at least 20 shipped themes, found {checked_theme_count}"
    );
}
