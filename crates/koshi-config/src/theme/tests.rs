//! Tests for the `themes/<name>.kdl` color-theme parser.

use std::path::Path;

use crate::error::ConfigError;
use crate::layer::PartialThemeConfig;
use crate::types::RgbColor;

use super::parse_theme;

/// Parses `source` as a theme file, panicking on error.
fn parse(source: &str) -> (PartialThemeConfig, Vec<String>) {
    parse_theme(Path::new("themes/midnight.kdl"), source).expect("valid theme")
}

#[test]
fn colors_parse_and_the_theme_is_left_unnamed() {
    let (theme, warnings) =
        parse("colors {\n    ramp-start \"#581c87\"\n    accent \"#a78bfa\"\n}");
    // The theme is named by its file, which this parser never sees; the loader
    // fills the name in from the path it read.
    assert_eq!(theme.name, None);
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
         bar-bg \"#0d0d0d\"\n\
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
    assert_eq!(c.bar_bg, Some(RgbColor::new(13, 13, 13)));
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
    let error = parse_theme(Path::new("themes/midnight.kdl"), "version 999")
        .expect_err("version newer than this build");
    assert!(matches!(error, ConfigError::Validation { key, .. } if key == "version"));
}

#[test]
fn a_syntax_error_is_a_parse_error() {
    let error = parse_theme(Path::new("themes/midnight.kdl"), "colors { accent \"#fff\"")
        .expect_err("unclosed block");
    assert!(matches!(error, ConfigError::Parse { .. }));
}

// -- adversarial: type confusion and exact warnings -----------------------

#[test]
fn a_bad_color_names_the_exact_reason_in_its_warning() {
    // The skip warning carries the underlying color-parse reason verbatim, not
    // just the field name.
    let (theme, warnings) = parse("colors {\n    ramp-start \"nothex\"\n}");
    assert_eq!(theme.colors.expect("colors present").ramp_start, None);
    assert_eq!(
        warnings,
        vec!["ignored `colors.ramp-start`: color `nothex` contains a non-hex digit".to_string()]
    );

    let (_, warnings) = parse("colors {\n    accent \"#fff\"\n}");
    assert_eq!(
        warnings,
        vec!["ignored `colors.accent`: color must be 6 hex digits (#RRGGBB), got 3".to_string()]
    );
}

#[test]
fn a_color_given_as_an_integer_is_skipped_as_a_non_string() {
    // A number where a hex string belongs is the wrong kind of value; it is
    // skipped with the shared "expected a string" reason and the default
    // color stands.
    let (theme, warnings) = parse("colors {\n    accent 5\n}");
    assert_eq!(theme.colors.expect("colors present").accent, None);
    assert_eq!(
        warnings,
        vec!["ignored `colors.accent`: expected a string".to_string()]
    );
}

#[test]
fn a_name_node_is_ignored_like_any_other_unknown_top_level_node() {
    // The theme's name comes from its file name, so a `name` node in the file
    // is not part of the schema and is passed over in silence.
    let (theme, warnings) = parse("name \"solarized\"\ncolors {\n    accent \"#ffffff\"\n}");
    assert_eq!(theme.name, None);
    assert_eq!(
        theme.colors.expect("colors present").accent,
        Some(RgbColor::new(0xff, 0xff, 0xff))
    );
    assert!(warnings.is_empty());
}

#[test]
fn a_repeated_color_role_keeps_the_last_value() {
    // Two entries for one role: the later one overwrites the earlier, with no
    // warning — KDL allows the repeat and the parser takes the final word.
    let (theme, warnings) = parse("colors {\n    accent \"#000000\"\n    accent \"#ffffff\"\n}");
    assert_eq!(
        theme.colors.expect("colors present").accent,
        Some(RgbColor::new(0xff, 0xff, 0xff))
    );
    assert!(warnings.is_empty());
}

#[test]
fn the_channel_boundaries_survive_the_parser() {
    let (theme, warnings) =
        parse("colors {\n    on-accent \"#000000\"\n    on-ramp \"#ffffff\"\n}");
    let colors = theme.colors.expect("colors present");
    assert_eq!(colors.on_accent, Some(RgbColor::new(0, 0, 0)));
    assert_eq!(colors.on_ramp, Some(RgbColor::new(0xff, 0xff, 0xff)));
    assert!(warnings.is_empty());
}

#[test]
fn an_empty_colors_block_sets_no_role() {
    // A `colors` block with no children is present but overrides nothing.
    let (theme, warnings) = parse("colors {\n}");
    let colors = theme.colors.expect("colors present");
    assert_eq!(colors.ramp_start, None);
    assert_eq!(colors.accent, None);
    assert!(warnings.is_empty());
}

#[test]
fn a_non_integer_version_is_a_validation_error() {
    // A garbage version value is a validation failure, not a silent skip.
    let error = parse_theme(Path::new("themes/midnight.kdl"), "version \"abc\"")
        .expect_err("string is not a version integer");
    match error {
        ConfigError::Validation { key, detail } => {
            assert_eq!(key, "version");
            assert_eq!(detail, "expected an integer");
        }
        other => panic!("expected a validation error, got {other:?}"),
    }
}

#[test]
fn a_comments_only_theme_is_treated_as_empty() {
    let (theme, warnings) = parse("// just a comment\n");
    assert_eq!(theme.name, None);
    assert!(theme.colors.is_none());
    assert!(warnings.is_empty());
}
