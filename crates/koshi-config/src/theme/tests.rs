//! Tests for the `theme.kdl` color-theme parser.

use std::path::Path;

use crate::error::ConfigError;
use crate::layer::PartialThemeConfig;
use crate::types::RgbColor;

use super::parse_theme;

/// Parses `source` as `theme.kdl`, panicking on error.
fn parse(source: &str) -> (PartialThemeConfig, Vec<String>) {
    parse_theme(Path::new("theme.kdl"), source).expect("valid theme")
}

#[test]
fn name_and_colors_parse() {
    let (theme, warnings) =
        parse("name \"midnight\"\ncolors {\n    ramp-start \"#581c87\"\n    accent \"#a78bfa\"\n}");
    assert_eq!(theme.name, Some("midnight".to_string()));
    let colors = theme.colors.expect("colors present");
    assert_eq!(colors.ramp_start, Some(RgbColor::new(0x58, 0x1c, 0x87)));
    assert_eq!(colors.accent, Some(RgbColor::new(0xa7, 0x8b, 0xfa)));
    // A role the file did not name keeps the lower layer's color.
    assert_eq!(colors.ramp_end, None);
    assert!(warnings.is_empty());
}

#[test]
fn every_color_role_parses() {
    let (theme, warnings) = parse(
        "colors {\n\
         ramp-start \"#010101\"\n\
         ramp-end \"#020202\"\n\
         on-ramp \"#030303\"\n\
         on-ramp-dim \"#040404\"\n\
         accent \"#050505\"\n\
         on-accent \"#060606\"\n\
         border-focused \"#070707\"\n\
         border-unfocused \"#080808\"\n\
         border-hover \"#090909\"\n\
         stack-header-fg \"#0a0a0a\"\n\
         stack-header-bg \"#0b0b0b\"\n\
         letterbox \"#0c0c0c\"\n\
         }",
    );
    let c = theme.colors.expect("colors present");
    assert_eq!(c.ramp_start, Some(RgbColor::new(1, 1, 1)));
    assert_eq!(c.ramp_end, Some(RgbColor::new(2, 2, 2)));
    assert_eq!(c.on_ramp, Some(RgbColor::new(3, 3, 3)));
    assert_eq!(c.on_ramp_dim, Some(RgbColor::new(4, 4, 4)));
    assert_eq!(c.accent, Some(RgbColor::new(5, 5, 5)));
    assert_eq!(c.on_accent, Some(RgbColor::new(6, 6, 6)));
    assert_eq!(c.border_focused, Some(RgbColor::new(7, 7, 7)));
    assert_eq!(c.border_unfocused, Some(RgbColor::new(8, 8, 8)));
    assert_eq!(c.border_hover, Some(RgbColor::new(9, 9, 9)));
    assert_eq!(c.stack_header_fg, Some(RgbColor::new(10, 10, 10)));
    assert_eq!(c.stack_header_bg, Some(RgbColor::new(11, 11, 11)));
    assert_eq!(c.letterbox, Some(RgbColor::new(12, 12, 12)));
    assert!(warnings.is_empty());
}

#[test]
fn a_bad_color_is_skipped_and_the_rest_apply() {
    let (theme, warnings) = parse("colors {\n    ramp-start \"nothex\"\n    accent \"#a78bfa\"\n}");
    let colors = theme.colors.expect("colors present");
    assert_eq!(colors.ramp_start, None);
    assert_eq!(colors.accent, Some(RgbColor::new(0xa7, 0x8b, 0xfa)));
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("colors.ramp-start"));
}

#[test]
fn an_unknown_color_role_warns() {
    let (_, warnings) = parse("colors {\n    foreground \"#ffffff\"\n}");
    assert_eq!(
        warnings,
        vec!["ignored unknown `colors.foreground`".to_string()]
    );
}

#[test]
fn a_bare_hex_without_a_hash_parses() {
    let (theme, _) = parse("colors {\n    accent \"a78bfa\"\n}");
    assert_eq!(
        theme.colors.expect("colors present").accent,
        Some(RgbColor::new(0xa7, 0x8b, 0xfa))
    );
}

#[test]
fn a_newer_schema_version_is_rejected() {
    let error = parse_theme(Path::new("theme.kdl"), "version 999")
        .expect_err("version newer than this build");
    assert!(matches!(error, ConfigError::Validation { key, .. } if key == "version"));
}

#[test]
fn a_syntax_error_is_a_parse_error() {
    let error = parse_theme(Path::new("theme.kdl"), "colors { accent \"#fff\"")
        .expect_err("unclosed block");
    assert!(matches!(error, ConfigError::Parse { .. }));
}
