//! Guards that the full "complete default" examples shipped in `config-docs/`
//! stay valid: each is fed through its real parser, so a schema change that
//! breaks a documented config fails here. `koshi.kdl` and `theme.kdl` must
//! parse with no field-partial warnings (every field is spelled correctly);
//! `keybinding.kdl` and the profile must parse cleanly.

use std::path::Path;

use koshi_config::app_config::parse_app_config;
use koshi_config::keybinding::parse_keybindings;
use koshi_config::profile::parse_profile;
use koshi_config::theme::parse_theme;

const KOSHI: &str = r#"
version 1
pane { min-cols 2; min-rows 1 }
scrollback { max-lines 10000; max-bytes 33554432 }
layout { new-pane-direction "right" }
mouse { border-resize #true; scroll-lines 3; wheel "scroll-scrollback" }
copy { trim-trailing-whitespace #true }
terminal { term "xterm-256color"; colorterm "truecolor"; default-shell "/bin/zsh" }
logging { enabled #false }
update { auto-check #true; check-interval-days 14; allow-prerelease #false }
"#;

const THEME: &str = r##"
version 1
name "default"
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
    let (_, warnings) = parse_app_config(Path::new("koshi.kdl"), KOSHI).expect("koshi.kdl parses");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

#[test]
fn theme_example_parses_without_warnings() {
    let (_, warnings) = parse_theme(Path::new("theme.kdl"), THEME).expect("theme.kdl parses");
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
