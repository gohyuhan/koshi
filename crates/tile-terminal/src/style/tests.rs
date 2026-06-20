//! Unit tests for cell styling.

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
            underline: false,
            reverse: false,
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
        }
    );
}
