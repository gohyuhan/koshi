//! Unit tests for the per-screen render state and its charset slots.

use super::*;
use crate::style::{Color, Style};

#[test]
fn charset_default_is_ascii() {
    assert_eq!(Charset::default(), Charset::Ascii);
}

#[test]
fn the_three_charsets_are_distinct() {
    let charsets = [Charset::Ascii, Charset::DecLineDrawing, Charset::Uk];
    for (i, a) in charsets.iter().enumerate() {
        for (j, b) in charsets.iter().enumerate() {
            assert_eq!(a == b, i == j);
        }
    }
}

#[test]
fn fresh_render_state_has_default_pen_all_ascii_slots_and_gl_on_g0() {
    let render = RenderState::fresh();
    assert_eq!(render.style, Style::default());
    assert_eq!(render.charsets, [Charset::Ascii; 4]);
    assert_eq!(render.gl, 0);
}

#[test]
fn render_states_differing_only_by_the_active_gl_slot_are_not_equal() {
    let on_g0 = RenderState::fresh();
    let mut on_g1 = RenderState::fresh();
    on_g1.gl = 1;
    assert_ne!(on_g0, on_g1);
}

#[test]
fn render_states_differing_only_by_a_charset_designation_are_not_equal() {
    let all_ascii = RenderState::fresh();
    let mut g1_line_drawing = RenderState::fresh();
    g1_line_drawing.charsets[1] = Charset::DecLineDrawing;
    assert_ne!(all_ascii, g1_line_drawing);
}

#[test]
fn render_states_differing_only_by_the_pen_are_not_equal() {
    let default_pen = RenderState::fresh();
    let mut colored_pen = RenderState::fresh();
    let mut style = Style::default();
    style.set_bg(Color::Indexed(4));
    colored_pen.style = style;
    assert_ne!(default_pen, colored_pen);
}
