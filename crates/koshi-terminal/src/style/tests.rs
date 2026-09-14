//! Unit tests for `Style`, `Color`, `AttrFlags`, and `UnderlineStyle`.

use super::*;

/// Read every attribute of `attribute_flags` in one value, in declaration order.
/// declared. Asserting on this pins all nine at once, so a setter that also
/// touches a flag it has no business touching fails here.
fn read_attribute_values(
    attribute_flags: AttrFlags,
) -> (
    bool,
    bool,
    UnderlineStyle,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
) {
    (
        attribute_flags.is_bold(),
        attribute_flags.is_italic(),
        attribute_flags.get_underline_style(),
        attribute_flags.is_reverse(),
        attribute_flags.is_faint(),
        attribute_flags.is_blinking(),
        attribute_flags.is_concealed(),
        attribute_flags.is_strikethrough(),
        attribute_flags.is_overlined(),
    )
}

#[test]
fn color_default_is_the_default_variant() {
    assert_eq!(Color::default(), Color::Default);
}

#[test]
fn attr_flags_default_is_all_false() {
    assert_eq!(
        read_attribute_values(AttrFlags::default()),
        (
            false,
            false,
            UnderlineStyle::None,
            false,
            false,
            false,
            false,
            false,
            false
        )
    );
}

#[test]
fn style_default_is_default_colors_and_no_attrs() {
    assert_eq!(
        Style::default(),
        Style {
            foreground_color: Color::Default,
            background_color: Color::Default,
            attributes: AttrFlags::default(),
            underline_color: None,
        }
    );
}

#[test]
fn set_fg_sets_only_the_foreground() {
    let mut style = Style::default();
    style.set_foreground_color(Color::Indexed(5));
    assert_eq!(
        style,
        Style {
            foreground_color: Color::Indexed(5),
            background_color: Color::Default,
            attributes: AttrFlags::default(),
            underline_color: None,
        }
    );
}

#[test]
fn set_bg_sets_only_the_background() {
    let mut style = Style::default();
    style.set_background_color(Color::Rgb(1, 2, 3));
    assert_eq!(
        style,
        Style {
            foreground_color: Color::Default,
            background_color: Color::Rgb(1, 2, 3),
            attributes: AttrFlags::default(),
            underline_color: None,
        }
    );
}

#[test]
fn attribute_setters_toggle_their_flag_independently() {
    let mut style = Style::default();
    style.set_bold(true);
    style.set_underline(UnderlineStyle::Single);
    assert_eq!(
        read_attribute_values(style.attributes),
        (
            true,
            false,
            UnderlineStyle::Single,
            false,
            false,
            false,
            false,
            false,
            false
        )
    );
    style.set_bold(false); // clears bold, leaves underline set
    assert_eq!(
        read_attribute_values(style.attributes),
        (
            false,
            false,
            UnderlineStyle::Single,
            false,
            false,
            false,
            false,
            false,
            false
        )
    );
}

#[test]
fn set_italic_and_set_reverse_set_their_flags() {
    let mut style = Style::default();
    style.set_italic(true);
    style.set_reverse(true);
    assert_eq!(
        read_attribute_values(style.attributes),
        (
            false,
            true,
            UnderlineStyle::None,
            true,
            false,
            false,
            false,
            false,
            false
        )
    );
}

#[test]
fn reset_restores_the_default_pen() {
    let mut style = Style::default();
    style.set_bold(true);
    style.set_foreground_color(Color::Indexed(9));
    style.set_background_color(Color::Rgb(4, 5, 6));
    style.reset_style();
    assert_eq!(style, Style::default());
}

#[test]
fn background_fill_style_keeps_only_the_background() {
    let mut style = Style::default();
    style.set_foreground_color(Color::Indexed(1));
    style.set_background_color(Color::Indexed(4));
    style.set_bold(true);
    style.set_underline(UnderlineStyle::Curly);
    style.set_underline_color(Some(Color::Indexed(2)));
    // The erase-fill style carries the background only — fg, attrs, and the
    // underline color reset.
    assert_eq!(
        style.get_background_fill_style(),
        Style {
            foreground_color: Color::Default,
            background_color: Color::Indexed(4),
            attributes: AttrFlags::default(),
            underline_color: None,
        }
    );
}

#[test]
fn style_getters_return_each_set_field() {
    let mut style = Style::default();
    style.set_foreground_color(Color::Indexed(1));
    style.set_background_color(Color::Indexed(2));
    style.set_bold(true);
    style.set_underline_color(Some(Color::Rgb(7, 8, 9)));

    assert_eq!(style.get_foreground_color(), Color::Indexed(1));
    assert_eq!(style.get_background_color(), Color::Indexed(2));
    assert_eq!(
        read_attribute_values(style.get_attributes()),
        (
            true,
            false,
            UnderlineStyle::None,
            false,
            false,
            false,
            false,
            false,
            false
        )
    );
    assert_eq!(style.get_underline_color(), Some(Color::Rgb(7, 8, 9)));
}

#[test]
fn attr_flags_getters_return_each_set_flag() {
    // A distinct on/off pattern per flag: any getter reading the wrong field
    // returns the mismatched value.
    let mut style = Style::default();
    style.set_bold(true);
    style.set_italic(false);
    style.set_underline(UnderlineStyle::Double);
    style.set_reverse(true);
    style.set_faint(false);
    style.set_blink(true);
    style.set_conceal(false);
    style.set_strike(true);
    style.set_overline(false);

    assert_eq!(
        read_attribute_values(style.get_attributes()),
        (
            true,
            false,
            UnderlineStyle::Double,
            true,
            false,
            true,
            false,
            true,
            false
        )
    );
}

#[test]
fn every_flag_can_be_set_at_once() {
    // All nine attributes on together: any storage that let two attributes
    // share a slot would lose one of them here.
    let mut style = Style::default();
    style.set_bold(true);
    style.set_italic(true);
    style.set_underline(UnderlineStyle::Dashed);
    style.set_reverse(true);
    style.set_faint(true);
    style.set_blink(true);
    style.set_conceal(true);
    style.set_strike(true);
    style.set_overline(true);

    assert_eq!(
        read_attribute_values(style.get_attributes()),
        (
            true,
            true,
            UnderlineStyle::Dashed,
            true,
            true,
            true,
            true,
            true,
            true
        )
    );
}

#[test]
fn every_underline_style_survives_the_other_flags_being_set() {
    // Each style written while all eight booleans are on: it must read back
    // intact and must not disturb any of them.
    for underline in [
        UnderlineStyle::None,
        UnderlineStyle::Single,
        UnderlineStyle::Double,
        UnderlineStyle::Curly,
        UnderlineStyle::Dotted,
        UnderlineStyle::Dashed,
    ] {
        let mut style = Style::default();
        style.set_bold(true);
        style.set_italic(true);
        style.set_reverse(true);
        style.set_faint(true);
        style.set_blink(true);
        style.set_conceal(true);
        style.set_strike(true);
        style.set_overline(true);
        style.set_underline(underline);

        assert_eq!(
            read_attribute_values(style.get_attributes()),
            (true, true, underline, true, true, true, true, true, true),
            "{underline:?}"
        );
    }
}

#[test]
fn setting_a_new_underline_style_replaces_the_previous_one() {
    // The styles are mutually exclusive, so the last one written is the one
    // that shows — a cell never draws two underlines.
    let mut style = Style::default();
    style.set_underline(UnderlineStyle::Dashed);
    style.set_underline(UnderlineStyle::Single);
    assert_eq!(
        style.get_attributes().get_underline_style(),
        UnderlineStyle::Single
    );
    style.set_underline(UnderlineStyle::None);
    assert_eq!(
        style.get_attributes().get_underline_style(),
        UnderlineStyle::None
    );
}

#[test]
fn clearing_one_flag_leaves_the_others_alone() {
    // Turning an attribute off must clear exactly that attribute.
    let mut style = Style::default();
    style.set_bold(true);
    style.set_italic(true);
    style.set_strike(true);
    style.set_bold(false);

    assert_eq!(
        read_attribute_values(style.get_attributes()),
        (
            false,
            true,
            UnderlineStyle::None,
            false,
            false,
            false,
            false,
            true,
            false
        )
    );
}

#[test]
fn reset_style_clears_the_underline_style_and_color_too() {
    let mut style = Style::default();
    style.set_underline(UnderlineStyle::Dotted);
    style.set_underline_color(Some(Color::Indexed(3)));
    style.reset_style();
    assert_eq!(style, Style::default());
}

#[test]
fn set_underline_color_none_restores_the_default() {
    let mut style = Style::default();
    style.set_underline_color(Some(Color::Rgb(1, 2, 3)));
    style.set_underline_color(None);
    assert_eq!(style.get_underline_color(), None);
    assert_eq!(style, Style::default());
}

#[test]
fn setting_a_flag_twice_then_clearing_it_once_turns_it_off() {
    let mut style = Style::default();
    style.set_bold(true);
    style.set_bold(true);
    style.set_bold(false);
    assert_eq!(style.get_attributes(), AttrFlags::default());
}

#[test]
fn debug_lists_the_attributes_that_are_on() {
    assert_eq!(format!("{:?}", AttrFlags::default()), "AttrFlags(none)");

    let mut style = Style::default();
    style.set_bold(true);
    style.set_underline(UnderlineStyle::Single);
    assert_eq!(
        format!("{:?}", style.get_attributes()),
        "AttrFlags(bold, underline)"
    );

    let mut style = Style::default();
    style.set_underline(UnderlineStyle::Curly);
    assert_eq!(
        format!("{:?}", style.get_attributes()),
        "AttrFlags(curly-underline)"
    );

    let mut style = Style::default();
    style.set_overline(true);
    style.set_italic(true);
    style.set_underline(UnderlineStyle::Dashed);
    assert_eq!(
        format!("{:?}", style.get_attributes()),
        "AttrFlags(italic, overline, dashed-underline)"
    );
}

#[test]
fn attr_flags_serialize_as_the_packed_word() {
    // `ESC[4;9m`: single underline (code 1 in bits 8-10) and strikethrough
    // (bit 6) — the 320 the type doc promises.
    let mut style = Style::default();
    style.set_underline(UnderlineStyle::Single);
    style.set_strike(true);
    let attrs = style.get_attributes();

    let serialized_attributes = serde_json::to_value(attrs).expect("attrs serialize");
    assert_eq!(serialized_attributes, serde_json::json!(320));
    let restored: AttrFlags =
        serde_json::from_value(serialized_attributes).expect("attrs deserialize");
    assert_eq!(restored, attrs);
}

#[test]
fn an_undefined_underline_code_deserializes_as_no_underline() {
    // Bits 8-10 hold 6: not a style `set_underline` ever writes. Every
    // getter reads it as `None`.
    let attrs: AttrFlags = serde_json::from_value(serde_json::json!(6 << 8)).expect("deserializes");
    assert_eq!(
        read_attribute_values(attrs),
        (
            false,
            false,
            UnderlineStyle::None,
            false,
            false,
            false,
            false,
            false,
            false
        )
    );
}

#[test]
fn style_round_trips_through_serde() {
    let mut style = Style::default();
    style.set_foreground_color(Color::Rgb(10, 20, 30));
    style.set_background_color(Color::Indexed(200));
    style.set_faint(true);
    style.set_underline(UnderlineStyle::Double);
    style.set_underline_color(Some(Color::Indexed(9)));

    let serialized_style = serde_json::to_value(style).expect("style serializes");
    assert_eq!(
        serialized_style,
        serde_json::json!({
            "fg": { "Rgb": [10, 20, 30] },
            "bg": { "Indexed": 200 },
            "attrs": (1 << 3) | (2 << 8),
            "underline_color": { "Indexed": 9 },
        })
    );
    let restored: Style = serde_json::from_value(serialized_style).expect("style deserializes");
    assert_eq!(restored, style);
}

#[test]
fn a_word_read_back_keeps_only_the_bits_the_getters_read() {
    // A spare bit and an undefined underline code both read as the default
    // through every getter, so a word carrying them equals the default.
    for word in [1u16 << 11, 1 << 15, 6 << 8, 7 << 8, 0xF800 | (7 << 8)] {
        let attrs: AttrFlags =
            serde_json::from_value(serde_json::json!(word)).expect("deserializes");
        assert_eq!(
            read_attribute_values(attrs),
            read_attribute_values(AttrFlags::default()),
            "word {word}"
        );
        assert_eq!(attrs, AttrFlags::default(), "word {word}");
    }

    // A defined word is kept whole: bold plus a single underline.
    let mut style = Style::default();
    style.set_bold(true);
    style.set_underline(UnderlineStyle::Single);
    let defined = style.get_attributes();
    let read_back: AttrFlags =
        serde_json::from_value(serde_json::to_value(defined).expect("serializes"))
            .expect("deserializes");
    assert_eq!(read_back, defined);
}
