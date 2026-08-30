//! Unit tests for `Style`, `Color`, `AttrFlags`, and `UnderlineStyle`.

use super::*;

/// Every attribute of `attrs` read out in one value, in the order they are
/// declared. Asserting on this pins all nine at once, so a setter that also
/// touches a flag it has no business touching fails here.
fn all(
    attrs: AttrFlags,
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
        attrs.bold(),
        attrs.italic(),
        attrs.underline(),
        attrs.reverse(),
        attrs.faint(),
        attrs.blink(),
        attrs.conceal(),
        attrs.strike(),
        attrs.overline(),
    )
}

#[test]
fn color_default_is_the_default_variant() {
    assert_eq!(Color::default(), Color::Default);
}

#[test]
fn attr_flags_default_is_all_false() {
    assert_eq!(
        all(AttrFlags::default()),
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
            fg: Color::Default,
            bg: Color::Default,
            attrs: AttrFlags::default(),
            underline_color: None,
        }
    );
}

#[test]
fn set_fg_sets_only_the_foreground() {
    let mut style = Style::default();
    style.set_fg(Color::Indexed(5));
    assert_eq!(
        style,
        Style {
            fg: Color::Indexed(5),
            bg: Color::Default,
            attrs: AttrFlags::default(),
            underline_color: None,
        }
    );
}

#[test]
fn set_bg_sets_only_the_background() {
    let mut style = Style::default();
    style.set_bg(Color::Rgb(1, 2, 3));
    assert_eq!(
        style,
        Style {
            fg: Color::Default,
            bg: Color::Rgb(1, 2, 3),
            attrs: AttrFlags::default(),
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
        all(style.attrs),
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
        all(style.attrs),
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
        all(style.attrs),
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
    style.set_fg(Color::Indexed(9));
    style.set_bg(Color::Rgb(4, 5, 6));
    style.reset();
    assert_eq!(style, Style::default());
}

#[test]
fn bg_fill_keeps_only_the_background() {
    let mut style = Style::default();
    style.set_fg(Color::Indexed(1));
    style.set_bg(Color::Indexed(4));
    style.set_bold(true);
    style.set_underline(UnderlineStyle::Curly);
    style.set_underline_color(Some(Color::Indexed(2)));
    // The erase-fill style carries the background only — fg, attrs, and the
    // underline color reset.
    assert_eq!(
        style.bg_fill(),
        Style {
            fg: Color::Default,
            bg: Color::Indexed(4),
            attrs: AttrFlags::default(),
            underline_color: None,
        }
    );
}

#[test]
fn style_getters_return_each_set_field() {
    let mut style = Style::default();
    style.set_fg(Color::Indexed(1));
    style.set_bg(Color::Indexed(2));
    style.set_bold(true);
    style.set_underline_color(Some(Color::Rgb(7, 8, 9)));

    assert_eq!(style.fg(), Color::Indexed(1));
    assert_eq!(style.bg(), Color::Indexed(2));
    assert_eq!(
        all(style.attrs()),
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
    assert_eq!(style.underline_color(), Some(Color::Rgb(7, 8, 9)));
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
        all(style.attrs()),
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
        all(style.attrs()),
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
            all(style.attrs()),
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
    assert_eq!(style.attrs().underline(), UnderlineStyle::Single);
    style.set_underline(UnderlineStyle::None);
    assert_eq!(style.attrs().underline(), UnderlineStyle::None);
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
        all(style.attrs()),
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
fn reset_clears_the_underline_style_and_color_too() {
    let mut style = Style::default();
    style.set_underline(UnderlineStyle::Dotted);
    style.set_underline_color(Some(Color::Indexed(3)));
    style.reset();
    assert_eq!(style, Style::default());
}

#[test]
fn set_underline_color_none_restores_the_default() {
    let mut style = Style::default();
    style.set_underline_color(Some(Color::Rgb(1, 2, 3)));
    style.set_underline_color(None);
    assert_eq!(style.underline_color(), None);
    assert_eq!(style, Style::default());
}

#[test]
fn setting_a_flag_twice_then_clearing_it_once_turns_it_off() {
    let mut style = Style::default();
    style.set_bold(true);
    style.set_bold(true);
    style.set_bold(false);
    assert_eq!(style.attrs(), AttrFlags::default());
}

#[test]
fn debug_lists_the_attributes_that_are_on() {
    assert_eq!(format!("{:?}", AttrFlags::default()), "AttrFlags(none)");

    let mut style = Style::default();
    style.set_bold(true);
    style.set_underline(UnderlineStyle::Single);
    assert_eq!(format!("{:?}", style.attrs()), "AttrFlags(bold, underline)");

    let mut style = Style::default();
    style.set_underline(UnderlineStyle::Curly);
    assert_eq!(format!("{:?}", style.attrs()), "AttrFlags(curly-underline)");

    let mut style = Style::default();
    style.set_overline(true);
    style.set_italic(true);
    style.set_underline(UnderlineStyle::Dashed);
    assert_eq!(
        format!("{:?}", style.attrs()),
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
    let attrs = style.attrs();

    let value = serde_json::to_value(attrs).expect("attrs serialize");
    assert_eq!(value, serde_json::json!(320));
    let restored: AttrFlags = serde_json::from_value(value).expect("attrs deserialize");
    assert_eq!(restored, attrs);
}

#[test]
fn an_undefined_underline_code_deserializes_as_no_underline() {
    // Bits 8-10 hold 6: not a style `set_underline` ever writes. Every
    // getter reads it as `None`.
    let attrs: AttrFlags = serde_json::from_value(serde_json::json!(6 << 8)).expect("deserializes");
    assert_eq!(
        all(attrs),
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
    style.set_fg(Color::Rgb(10, 20, 30));
    style.set_bg(Color::Indexed(200));
    style.set_faint(true);
    style.set_underline(UnderlineStyle::Double);
    style.set_underline_color(Some(Color::Indexed(9)));

    let value = serde_json::to_value(style).expect("style serializes");
    assert_eq!(
        value,
        serde_json::json!({
            "fg": { "Rgb": [10, 20, 30] },
            "bg": { "Indexed": 200 },
            "attrs": (1 << 3) | (2 << 8),
            "underline_color": { "Indexed": 9 },
        })
    );
    let restored: Style = serde_json::from_value(value).expect("style deserializes");
    assert_eq!(restored, style);
}
