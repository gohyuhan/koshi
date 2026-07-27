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
    assert_eq!(style.fg, Color::Indexed(5));
    assert_eq!(style.bg, Color::Default); // background untouched
    assert_eq!(style.attrs, AttrFlags::default()); // attributes untouched
}

#[test]
fn set_bg_sets_only_the_background() {
    let mut style = Style::default();
    style.set_bg(Color::Rgb(1, 2, 3));
    assert_eq!(style.bg, Color::Rgb(1, 2, 3));
    assert_eq!(style.fg, Color::Default);
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
    // The erase-fill style carries the background only — fg + attrs reset.
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
    assert!(style.attrs().bold());
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

    let attrs = style.attrs();
    assert!(attrs.bold());
    assert!(!attrs.italic());
    assert_eq!(attrs.underline(), UnderlineStyle::Double);
    assert!(attrs.reverse());
    assert!(!attrs.faint());
    assert!(attrs.blink());
    assert!(!attrs.conceal());
    assert!(attrs.strike());
    assert!(!attrs.overline());
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

    let attrs = style.attrs();
    assert!(attrs.bold());
    assert!(attrs.italic());
    assert_eq!(attrs.underline(), UnderlineStyle::Dashed);
    assert!(attrs.reverse());
    assert!(attrs.faint());
    assert!(attrs.blink());
    assert!(attrs.conceal());
    assert!(attrs.strike());
    assert!(attrs.overline());
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

        let attrs = style.attrs();
        assert_eq!(attrs.underline(), underline);
        assert!(attrs.bold());
        assert!(attrs.italic());
        assert!(attrs.reverse());
        assert!(attrs.faint());
        assert!(attrs.blink());
        assert!(attrs.conceal());
        assert!(attrs.strike());
        assert!(attrs.overline());
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

    let attrs = style.attrs();
    assert!(!attrs.bold());
    assert!(attrs.italic());
    assert!(attrs.strike());
}
