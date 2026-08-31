//! Guards the `## Full example` block of every page in `config-docs/`. Each
//! block is read straight out of the markdown, fed through its real parser, and
//! checked against the built-in defaults the page says it documents. `koshi.kdl`
//! and the theme file must parse with no warnings at all; a misspelled field
//! name is reported as an `ignored ...` warning rather than an error.

use std::path::Path;

use koshi_config::app_config::parse_app_config;
use koshi_config::key::Leader;
use koshi_config::keybinding::parse_keybindings;
use koshi_config::layer::{merge_client, merge_server};
use koshi_config::profile::parse_profile;
use koshi_config::theme::parse_theme;
use koshi_config::types::{
    default_mode_bindings, ClientConfig, ColorPalette, ServerConfig, DEFAULT_THEME,
};

/// The KDL text of the fenced block under the `## Full example` heading of
/// `config-docs/<page>` — the complete example the docs tell a user to copy.
///
/// Every `\r\n` in the page becomes `\n` before the headings and fences are
/// looked for, so a checkout that stores the page with Windows line endings
/// reads the same as one that stores it with Unix line endings.
///
/// # Panics
/// Panics when the page cannot be read, carries no `## Full example` heading,
/// or has no closed ```` ```kdl ```` block after that heading.
fn full_example(page: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config-docs")
        .join(page);
    let markdown = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} is readable: {err}", path.display()))
        .replace("\r\n", "\n");
    let after_heading = markdown
        .split_once("\n## Full example\n")
        .unwrap_or_else(|| panic!("{page} has a `## Full example` heading"))
        .1;
    let inside_fence = after_heading
        .split_once("```kdl\n")
        .unwrap_or_else(|| panic!("{page} opens a kdl block under `## Full example`"))
        .1;
    inside_fence
        .split_once("\n```")
        .unwrap_or_else(|| panic!("{page} closes its kdl block"))
        .0
        .to_string()
}

#[test]
fn koshi_example_parses_without_warnings() {
    let source = full_example("koshi.md");
    let file = parse_app_config(Path::new("koshi.kdl"), &source).expect("koshi.kdl parses");
    assert!(
        file.warnings.is_empty(),
        "unexpected warnings: {:?}",
        file.warnings
    );
    // The documented example names the built-in theme.
    assert_eq!(file.theme, Some(DEFAULT_THEME.to_string()));
    // Every value it spells out is the built-in default: folding its layer
    // onto the defaults leaves both sides unchanged.
    assert_eq!(
        merge_server(ServerConfig::default(), vec![file.layer.clone()]),
        ServerConfig::default()
    );
    assert_eq!(
        merge_client(ClientConfig::default(), vec![file.layer]),
        ClientConfig::default()
    );
}

#[test]
fn theme_example_parses_without_warnings() {
    let source = full_example("theme.md");
    let (theme, warnings) =
        parse_theme(Path::new("themes/default.kdl"), &source).expect("theme file parses");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // The page documents every color at its default value.
    let colors = theme.colors.expect("the example sets a `colors` block");
    let stock = ColorPalette::default();
    for (role, parsed, expected) in [
        ("ramp-start", colors.ramp_start, stock.ramp_start),
        ("ramp-end", colors.ramp_end, stock.ramp_end),
        ("on-ramp", colors.on_ramp, stock.on_ramp),
        ("on-ramp-dim", colors.on_ramp_dim, stock.on_ramp_dim),
        ("accent", colors.accent, stock.accent),
        ("on-accent", colors.on_accent, stock.on_accent),
        ("bar-bg", colors.bar_bg, stock.bar_bg),
        (
            "border-focused",
            colors.border_focused,
            stock.border_focused,
        ),
        (
            "border-unfocused",
            colors.border_unfocused,
            stock.border_unfocused,
        ),
        ("border-hover", colors.border_hover, stock.border_hover),
        (
            "stack-header-fg",
            colors.stack_header_fg,
            stock.stack_header_fg,
        ),
        (
            "stack-header-bg",
            colors.stack_header_bg,
            stock.stack_header_bg,
        ),
        ("letterbox", colors.letterbox, stock.letterbox),
    ] {
        assert_eq!(parsed, Some(expected), "documented `{role}`");
    }
}

#[test]
fn keybinding_example_parses() {
    let source = full_example("keybinding.md");
    let layer =
        parse_keybindings(Path::new("keybinding.kdl"), &source).expect("keybinding.kdl parses");

    // The page documents the complete built-in keymap: the layer it parses to
    // is the shipped default table, key for key.
    assert_eq!(layer.chord_timeout_ms, Some(500));
    assert_eq!(layer.which_key_delay_ms, Some(300));
    assert_eq!(layer.max_chord_depth, Some(4));
    assert_eq!(layer.leader, Some(Leader::default()));
    assert_eq!(layer.unlock_alternative, None);
    assert_eq!(layer.modes, Some(default_mode_bindings(Leader::default())));
}

#[test]
fn profile_example_parses() {
    let source = full_example("profile.md");
    let template = parse_profile(Path::new("profile/dev.kdl"), &source).expect("profile parses");

    // Two tabs; the `focus` marker on the second one selects it at open.
    assert_eq!(template.tabs.len(), 2);
    assert_eq!(template.focused_tab, 1);
    assert!(!template.locked);
    // The editor pane carries `focus` and wins the first tab. The stack tab
    // marks no pane `focus` and falls back to the first visible leaf — the
    // `expanded` stack member, `htop`, at index 1.
    assert_eq!(template.tabs[0].focused_leaf, 0);
    assert_eq!(template.tabs[1].focused_leaf, 1);
}

/// Every ready-made theme shipped in `themes-example/` must parse with no
/// warnings and set all thirteen color roles.
///
/// The parser skips an unknown role name with a warning instead of rejecting
/// the file, and an unset role keeps that part of the chrome at koshi's default
/// color.
#[test]
fn every_shipped_example_theme_is_complete_and_warning_free() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes-example");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("themes-example directory exists") {
        let path = entry.expect("readable directory entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("kdl") {
            continue;
        }
        let name = path.file_name().expect("a file name").to_string_lossy();
        let source = std::fs::read_to_string(&path).expect("theme file is readable");
        let (theme, warnings) = parse_theme(&path, &source)
            .unwrap_or_else(|err| panic!("{name} does not parse: {err}"));
        assert!(warnings.is_empty(), "{name} has warnings: {warnings:?}");

        let colors = theme
            .colors
            .unwrap_or_else(|| panic!("{name} has no `colors` block"));
        for (role, set) in [
            ("ramp-start", colors.ramp_start.is_some()),
            ("ramp-end", colors.ramp_end.is_some()),
            ("on-ramp", colors.on_ramp.is_some()),
            ("on-ramp-dim", colors.on_ramp_dim.is_some()),
            ("accent", colors.accent.is_some()),
            ("on-accent", colors.on_accent.is_some()),
            ("bar-bg", colors.bar_bg.is_some()),
            ("border-focused", colors.border_focused.is_some()),
            ("border-unfocused", colors.border_unfocused.is_some()),
            ("border-hover", colors.border_hover.is_some()),
            ("stack-header-fg", colors.stack_header_fg.is_some()),
            ("stack-header-bg", colors.stack_header_bg.is_some()),
            ("letterbox", colors.letterbox.is_some()),
        ] {
            assert!(set, "{name} does not set `{role}`");
        }
        checked += 1;
    }
    assert!(
        checked >= 20,
        "expected at least 20 shipped themes, found {checked}"
    );
}
