//! Guards that the full "complete default" examples shipped in `config-docs/`
//! stay valid: each is fed through its real parser, so a schema change that
//! breaks a documented config fails here. `koshi.kdl` and the theme file must
//! parse with no field-partial warnings (every field is spelled correctly);
//! `keybinding.kdl` and the profile must parse cleanly.

use std::path::Path;

use koshi_config::app_config::parse_app_config;
use koshi_config::keybinding::parse_keybindings;
use koshi_config::profile::parse_profile;
use koshi_config::theme::parse_theme;

const KOSHI: &str = r#"
version 1
theme "default"
pane { min-cols 2; min-rows 1 }
scrollback { max-lines 10000; max-bytes 33554432; scroll-on-input #true }
layout { new-pane-direction "right" }
mouse { border-resize #true; scroll-lines 3; wheel "scroll-scrollback" }
copy { trim-trailing-whitespace #true }
terminal { term "xterm-256color"; colorterm "truecolor"; default-shell "/bin/zsh" }
logging { enabled #false; level "warning"; format "pretty" }
update { auto-check #true; check-interval-days 14; allow-prerelease #false }
"#;

const THEME: &str = r##"
version 1
colors {
    ramp-start "#d0a5ff"
    ramp-end "#7dbcff"
    on-ramp "#12091f"
    on-ramp-dim "#f0ecfa"
    accent "#f5c2ff"
    on-accent "#1e1033"
    bar-bg "#000000"
    border-focused "#00afd7"
    border-unfocused "#585858"
    border-hover "#af5fff"
    stack-header-fg "#f4f1fa"
    stack-header-bg "#300f4a"
    letterbox "#585858"
}
"##;

const KEYBINDING: &str = r#"
version 1
chord-timeout-ms 500
which-key-delay-ms 300
max-chord-depth 4
leader "C-"
mode "normal" {
    bind "<C-l>" "core:lock"
    bind "<leader>q" "core:quit"
    bind "<leader>g" "core:mouse-select"
    bind "<leader>p n" "core:new-pane"
    bind "<leader>p h" "core:new-pane-left"
    bind "<leader>p j" "core:new-pane-down"
    bind "<leader>p k" "core:new-pane-up"
    bind "<leader>p l" "core:new-pane-right"
    bind "<leader>p x" "core:close-pane-tree"
    bind "<leader>p <Left>" "core:focus-pane-left"
    bind "<leader>p <Down>" "core:focus-pane-down"
    bind "<leader>p <Up>" "core:focus-pane-up"
    bind "<leader>p <Right>" "core:focus-pane-right"
    bind "<leader>s h" "core:resize-pane-left"
    bind "<leader>s j" "core:resize-pane-down"
    bind "<leader>s k" "core:resize-pane-up"
    bind "<leader>s l" "core:resize-pane-right"
    bind "<A-f>" "core:toggle-pane-fullscreen"
    bind "<A-h>" "core:focus-pane-left"
    bind "<A-j>" "core:focus-pane-down"
    bind "<A-k>" "core:focus-pane-up"
    bind "<A-l>" "core:focus-pane-right"
    bind "<A-t>" "core:new-tab"
    bind "<Tab>" "core:next-tab"
    bind "<S-Tab>" "core:previous-tab"
    remove "<C-b>"
    bind "<leader>d" "core:close-pane-tree"
}
mode "locked" {
    bind "<C-l>" "core:unlock"
    bind "<leader>q" "core:quit"
    bind "<leader>g" "core:mouse-select"
}
"#;

const PROFILE: &str = r#"
version 1
tab {
    horizontal {
        pane {
            command "nvim" "src/main.rs"
            cwd "/home/me/proj"
            env "RUST_LOG" "debug"
            env "NO_COLOR" "1"
            size "60%"
            focus
        }
        vertical {
            size "40%"
            pane {
                command "cargo" "watch" "-x" "test"
                cwd "/home/me/proj"
                weight 2
                min 5
                preferred 20
            }
            pane {
                cwd "/home/me/proj"
                weight 1
            }
        }
    }
}
tab {
    focus
    stack {
        pane { command "journalctl" "-f" }
        pane { command "htop"; expanded }
    }
}
"#;

#[test]
fn koshi_example_parses_without_warnings() {
    let file = parse_app_config(Path::new("koshi.kdl"), KOSHI).expect("koshi.kdl parses");
    assert!(
        file.warnings.is_empty(),
        "unexpected warnings: {:?}",
        file.warnings
    );
    // The documented example names the built-in theme.
    assert_eq!(file.theme, Some("default".to_string()));
}

#[test]
fn theme_example_parses_without_warnings() {
    let (_, warnings) =
        parse_theme(Path::new("themes/default.kdl"), THEME).expect("theme file parses");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

#[test]
fn keybinding_example_parses() {
    parse_keybindings(Path::new("keybinding.kdl"), KEYBINDING).expect("keybinding.kdl parses");
}

#[test]
fn profile_example_parses() {
    parse_profile(Path::new("profile/dev.kdl"), PROFILE).expect("profile parses");
}

/// Every ready-made theme shipped in `themes-example/` must parse cleanly and
/// set all thirteen color roles.
///
/// These files are meant to be copied straight into a config directory, so a
/// typo'd role name — which the parser skips with a warning rather than
/// rejecting — would ship a theme that silently draws one part of the chrome in
/// koshi's default color. Requiring zero warnings turns that into a failure
/// here instead.
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
        // Named one by one so a failure says which role is missing.
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
