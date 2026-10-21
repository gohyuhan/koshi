//! Tests for the `themes/<name>.kdl` color-theme parser.

use super::*;

use crate::types::SCHEMA_VERSION;

/// Parses `theme_text` as a theme file, panicking on error.
fn parse_theme_text(theme_text: &str) -> (PartialThemeConfig, Vec<String>) {
    let theme_text_with_version = if theme_text
        .lines()
        .any(|line| line.trim_start().starts_with("version "))
    {
        theme_text.to_string()
    } else {
        format!("version 1\n{theme_text}")
    };
    parse_theme(Path::new("themes/midnight.kdl"), &theme_text_with_version).expect("valid theme")
}

/// A theme layer with no name and the color overrides `colors`.
fn build_unnamed_theme(colors: PartialColorPalette) -> PartialThemeConfig {
    PartialThemeConfig {
        theme_name: None,
        colors: Some(colors),
    }
}

/// The key and the detail of the validation error that parsing `theme_text`
/// gives.
fn parse_theme_validation_error(theme_text: &str) -> (String, String) {
    match parse_theme(Path::new("themes/midnight.kdl"), theme_text) {
        Err(ConfigError::Validation {
            config_key,
            validation_detail,
        }) => (config_key, validation_detail),
        other_parse_result => panic!("expected a validation error, got {other_parse_result:?}"),
    }
}

#[test]
fn missing_version_is_a_validation_error() {
    assert_eq!(
        parse_theme_validation_error("colors {}"),
        (
            "version".to_string(),
            "file must declare `version`".to_string()
        )
    );
}

#[test]
fn colors_parse_and_the_theme_is_left_unnamed() {
    let (parsed_theme, parse_warnings) =
        parse_theme_text("colors {\n    ramp-start \"#581c87\"\n    accent \"#a78bfa\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette {
            ramp_start: Some(RgbColor::from_channels(0x58, 0x1c, 0x87)),
            accent: Some(RgbColor::from_channels(0xa7, 0x8b, 0xfa)),
            ..PartialColorPalette::default()
        })
    );
    assert!(parse_warnings.is_empty());
}

#[test]
fn every_color_role_parses() {
    let (parsed_theme, parse_warnings) = parse_theme_text(
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
    let colors = parsed_theme.colors.expect("colors present");
    assert_eq!(colors.ramp_start, Some(RgbColor::from_channels(1, 1, 1)));
    assert_eq!(colors.ramp_end, Some(RgbColor::from_channels(2, 2, 2)));
    assert_eq!(colors.on_ramp, Some(RgbColor::from_channels(3, 3, 3)));
    assert_eq!(colors.on_ramp_dim, Some(RgbColor::from_channels(4, 4, 4)));
    assert_eq!(colors.accent, Some(RgbColor::from_channels(5, 5, 5)));
    assert_eq!(colors.on_accent, Some(RgbColor::from_channels(6, 6, 6)));
    assert_eq!(
        colors.border_focused,
        Some(RgbColor::from_channels(7, 7, 7))
    );
    assert_eq!(
        colors.border_unfocused,
        Some(RgbColor::from_channels(8, 8, 8))
    );
    assert_eq!(colors.border_hover, Some(RgbColor::from_channels(9, 9, 9)));
    assert_eq!(
        colors.stack_header_fg,
        Some(RgbColor::from_channels(10, 10, 10))
    );
    assert_eq!(
        colors.stack_header_bg,
        Some(RgbColor::from_channels(11, 11, 11))
    );
    assert_eq!(colors.letterbox, Some(RgbColor::from_channels(12, 12, 12)));
    assert_eq!(colors.bar_bg, Some(RgbColor::from_channels(13, 13, 13)));
    assert!(parse_warnings.is_empty());
}

#[test]
fn a_bad_color_is_skipped_and_the_rest_apply() {
    let (parsed_theme, parse_warnings) =
        parse_theme_text("colors {\n    ramp-start \"nothex\"\n    accent \"#a78bfa\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette {
            accent: Some(RgbColor::from_channels(0xa7, 0x8b, 0xfa)),
            ..PartialColorPalette::default()
        })
    );
    assert_eq!(
        parse_warnings,
        ["ignored `colors.ramp-start`: color `nothex` contains a non-hex digit"]
    );
}

#[test]
fn an_unknown_color_role_warns() {
    let (_, parse_warnings) = parse_theme_text("colors {\n    foreground \"#ffffff\"\n}");
    assert_eq!(
        parse_warnings,
        vec![
            "ignored unknown key `colors.foreground`; did you mean `colors.ramp-end`?".to_string()
        ]
    );
}

#[test]
fn a_bare_hex_without_a_hash_parses() {
    let (parsed_theme, parse_warnings) = parse_theme_text("colors {\n    accent \"a78bfa\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette {
            accent: Some(RgbColor::from_channels(0xa7, 0x8b, 0xfa)),
            ..PartialColorPalette::default()
        })
    );
    assert!(parse_warnings.is_empty());
}

#[test]
fn a_newer_schema_version_is_rejected() {
    assert_eq!(
        parse_theme_validation_error("version 999"),
        (
            "version".to_string(),
            format!(
                "config schema version 999 is newer than this koshi supports ({SCHEMA_VERSION})"
            )
        )
    );
}

#[test]
fn a_version_with_children_is_rejected() {
    assert_eq!(
        parse_theme_validation_error("version 1 {}"),
        (
            "version".to_string(),
            "`version` takes no children".to_string()
        )
    );
}

#[test]
fn a_syntax_error_is_a_parse_error() {
    let config_error = parse_theme(Path::new("themes/midnight.kdl"), "colors { accent \"#fff\"")
        .expect_err("unclosed block");

    let ConfigError::Parse {
        config_path,
        parse_error_detail,
    } = config_error
    else {
        panic!("expected a parse error, got {config_error:?}");
    };
    assert_eq!(config_path, "themes/midnight.kdl");
    assert_eq!(parse_error_detail, "No closing '}' for child block");
}

// -- adversarial: type confusion and exact warnings -----------------------

#[test]
fn a_color_given_as_an_integer_is_skipped_as_a_non_string() {
    let (parsed_theme, parse_warnings) = parse_theme_text("colors {\n    accent 5\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette::default())
    );
    assert_eq!(
        parse_warnings,
        ["ignored `colors.accent`: expected a string"]
    );
}

#[test]
fn a_name_node_warns_because_the_file_name_owns_the_theme_name() {
    let (parsed_theme, parse_warnings) =
        parse_theme_text("name \"solarized\"\ncolors {\n    accent \"#ffffff\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette {
            accent: Some(RgbColor::from_channels(0xff, 0xff, 0xff)),
            ..PartialColorPalette::default()
        })
    );
    assert_eq!(
        parse_warnings,
        ["ignored unknown key `name`; did you mean `colors`?"]
    );
}

#[test]
fn a_repeated_color_role_keeps_the_final_value() {
    let (parsed_theme, parse_warnings) =
        parse_theme_text("colors {\n    accent \"#000000\"\n    accent \"#ffffff\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette {
            accent: Some(RgbColor::from_channels(0xff, 0xff, 0xff)),
            ..PartialColorPalette::default()
        })
    );
    assert!(parse_warnings.is_empty());
}

#[test]
fn the_channel_boundaries_survive_the_parser() {
    let (parsed_theme, parse_warnings) =
        parse_theme_text("colors {\n    on-accent \"#000000\"\n    on-ramp \"#ffffff\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette {
            on_ramp: Some(RgbColor::from_channels(0xff, 0xff, 0xff)),
            on_accent: Some(RgbColor::from_channels(0, 0, 0)),
            ..PartialColorPalette::default()
        })
    );
    assert!(parse_warnings.is_empty());
}

#[test]
fn an_empty_colors_block_sets_no_role() {
    let (parsed_theme, parse_warnings) = parse_theme_text("colors {\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette::default())
    );
    assert!(parse_warnings.is_empty());
}

#[test]
fn a_non_integer_version_is_a_validation_error() {
    assert_eq!(
        parse_theme_validation_error("version \"abc\""),
        (
            "version".to_string(),
            "`version` must be an integer from 1 to 4294967295".to_string()
        )
    );
}

#[test]
fn a_comments_only_theme_is_treated_as_empty() {
    let (parsed_theme, parse_warnings) = parse_theme_text("// just a comment\n");
    assert_eq!(parsed_theme, PartialThemeConfig::default());
    assert!(parse_warnings.is_empty());
}

#[test]
fn a_repeated_colors_block_warns_and_the_first_one_stands() {
    let (parsed_theme, parse_warnings) = parse_theme_text(
        "colors {\n    accent \"#000000\"\n}\ncolors {\n    accent \"#ffffff\"\n    letterbox \"#123456\"\n}",
    );
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette {
            accent: Some(RgbColor::from_channels(0, 0, 0)),
            ..PartialColorPalette::default()
        })
    );
    assert_eq!(parse_warnings, ["ignored duplicate `colors` section"]);
}

#[test]
fn a_repeated_version_is_a_validation_error() {
    assert_eq!(
        parse_theme_validation_error("version 1\nversion 1"),
        (
            "version".to_string(),
            "`version` is declared more than once".to_string()
        )
    );
}

#[test]
fn version_zero_is_rejected() {
    assert_eq!(
        parse_theme_validation_error("version 0"),
        (
            "version".to_string(),
            "config schema version must be at least 1".to_string()
        )
    );
}

#[test]
fn a_negative_version_is_rejected() {
    assert_eq!(
        parse_theme_validation_error("version -1"),
        (
            "version".to_string(),
            "`version` must be an integer from 1 to 4294967295".to_string()
        )
    );
}

#[test]
fn a_version_with_two_values_is_rejected() {
    assert_eq!(
        parse_theme_validation_error("version 1 2"),
        (
            "version".to_string(),
            "`version` takes exactly one integer argument".to_string()
        )
    );
}

#[test]
fn a_colors_node_without_a_block_sets_no_role() {
    let (parsed_theme, parse_warnings) = parse_theme_text("colors");
    assert_eq!(parsed_theme.colors, Some(PartialColorPalette::default()));
    assert!(parse_warnings.is_empty());
}

#[test]
fn a_color_carrying_a_child_block_is_skipped() {
    let (parsed_theme, parse_warnings) =
        parse_theme_text("colors {\n    accent \"#ffffff\" {\n        shade\n    }\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette::default())
    );
    assert_eq!(
        parse_warnings,
        ["ignored `colors.accent`: takes no children"]
    );
}

#[test]
fn a_color_that_is_not_exactly_one_value_is_skipped() {
    let (parsed_theme, parse_warnings) =
        parse_theme_text("colors {\n    accent \"#ffffff\" \"#000000\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette::default())
    );
    assert_eq!(
        parse_warnings,
        ["ignored `colors.accent`: expected exactly one value".to_string()]
    );

    let (parsed_theme, parse_warnings) =
        parse_theme_text("colors {\n    accent value=\"#ffffff\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette::default())
    );
    assert_eq!(
        parse_warnings,
        ["ignored `colors.accent`: expected exactly one value".to_string()]
    );
}

#[test]
fn uppercase_hex_digits_parse() {
    let (parsed_theme, parse_warnings) = parse_theme_text("colors {\n    accent \"#A78BFA\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette {
            accent: Some(RgbColor::from_channels(0xa7, 0x8b, 0xfa)),
            ..PartialColorPalette::default()
        })
    );
    assert!(parse_warnings.is_empty());
}

#[test]
fn an_empty_color_string_is_skipped() {
    let (parsed_theme, parse_warnings) = parse_theme_text("colors {\n    accent \"\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette::default())
    );
    assert_eq!(
        parse_warnings,
        ["ignored `colors.accent`: color must be 6 hex digits (#RRGGBB), got 0"]
    );
}

#[test]
fn a_seven_digit_color_is_skipped() {
    let (parsed_theme, parse_warnings) = parse_theme_text("colors {\n    accent \"#1234567\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette::default())
    );
    assert_eq!(
        parse_warnings,
        ["ignored `colors.accent`: color must be 6 hex digits (#RRGGBB), got 7"]
    );
}

#[test]
fn a_six_character_color_of_multi_byte_characters_is_skipped() {
    // Six characters, twelve bytes. The digit check rejects the value before
    // any byte slicing runs.
    let (parsed_theme, parse_warnings) =
        parse_theme_text("colors {\n    accent \"#\u{ff}\u{ff}\u{ff}\u{ff}\u{ff}\u{ff}\"\n}");
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette::default())
    );
    assert_eq!(
        parse_warnings,
        [format!(
            "ignored `colors.accent`: color `{}` contains a non-hex digit",
            "\u{ff}".repeat(6)
        )]
    );
}

#[test]
fn warnings_come_out_in_file_order() {
    let (parsed_theme, parse_warnings) = parse_theme_text(
        "name \"solarized\"\ncolors {\n    foreground \"#ffffff\"\n}\ncolors {\n    accent \"#ffffff\"\n}",
    );
    assert_eq!(
        parsed_theme,
        build_unnamed_theme(PartialColorPalette::default())
    );
    assert_eq!(
        parse_warnings,
        [
            "ignored unknown key `name`; did you mean `colors`?",
            "ignored unknown key `colors.foreground`; did you mean `colors.ramp-end`?",
            "ignored duplicate `colors` section",
        ]
    );
}

#[test]
fn a_value_on_the_colors_line_is_warned_about_and_ignored() {
    let (parsed_theme, parse_warnings) =
        parse_theme_text("colors \"oops\" {\n    accent \"#ff0000\"\n}");
    assert_eq!(
        parse_warnings,
        ["ignored `colors` value: a section takes a `{ … }` block"]
    );
    assert_eq!(
        parsed_theme.colors,
        Some(PartialColorPalette {
            accent: Some(RgbColor::from_channels(0xff, 0x00, 0x00)),
            ..PartialColorPalette::default()
        })
    );
}
