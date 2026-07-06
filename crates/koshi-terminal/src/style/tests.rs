//! Unit tests for `Style`, `Color`, `AttrFlags`, and `UnderlineStyle`.

use super::*;

#[test]
fn color_default_is_the_default_variant() {
    assert_eq!(Color::default(), Color::Default);
}

#[test]
fn attr_flags_default_is_all_false() {
    assert_eq!(
        AttrFlags::default(),
        AttrFlags {
            bold: false,
            italic: false,
            underline: UnderlineStyle::None,
            reverse: false,
            faint: false,
            blink: false,
            conceal: false,
            strike: false,
            overline: false,
        }
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
        style.attrs,
        AttrFlags {
            bold: true,
            italic: false,
            underline: UnderlineStyle::Single,
            reverse: false,
            faint: false,
            blink: false,
            conceal: false,
            strike: false,
            overline: false,
        }
    );
    style.set_bold(false); // clears bold, leaves underline set
    assert_eq!(
        style.attrs,
        AttrFlags {
            bold: false,
            italic: false,
            underline: UnderlineStyle::Single,
            reverse: false,
            faint: false,
            blink: false,
            conceal: false,
            strike: false,
            overline: false,
        }
    );
}

#[test]
fn set_italic_and_set_reverse_set_their_flags() {
    let mut style = Style::default();
    style.set_italic(true);
    style.set_reverse(true);
    assert_eq!(
        style.attrs,
        AttrFlags {
            bold: false,
            italic: true,
            underline: UnderlineStyle::None,
            reverse: true,
            faint: false,
            blink: false,
            conceal: false,
            strike: false,
            overline: false,
        }
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
