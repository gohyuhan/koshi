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

#[test]
fn render_state_serializes_charsets_by_name_and_gl_as_a_number() {
    let mut render = RenderState::fresh();
    render.charsets[1] = Charset::DecLineDrawing;
    render.charsets[2] = Charset::Uk;
    render.gl = 1;

    let value = serde_json::to_value(render).expect("render state serializes");
    assert_eq!(
        value["charsets"],
        serde_json::json!(["Ascii", "DecLineDrawing", "Uk", "Ascii"])
    );
    assert_eq!(value["gl"], serde_json::json!(1));
    assert_eq!(
        value["style"],
        serde_json::to_value(Style::default()).unwrap()
    );

    let restored: RenderState = serde_json::from_value(value).expect("render state deserializes");
    assert_eq!(restored, render);
}

#[test]
fn an_unknown_charset_name_fails_to_deserialize() {
    let error = serde_json::from_value::<Charset>(serde_json::json!("Latin1")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "unknown variant `Latin1`, expected one of `Ascii`, `DecLineDrawing`, `Uk`"
    );
}

#[test]
fn a_gl_slot_past_g3_is_refused() {
    // `gl` indexes the four charset slots, so a fourth slot has no charset to
    // read and would panic on the first printed byte.
    let mut value = serde_json::to_value(RenderState::fresh()).expect("render state serializes");
    value["gl"] = serde_json::json!(4);

    let error = serde_json::from_value::<RenderState>(value).unwrap_err();

    assert_eq!(error.to_string(), "GL slot must be 0-3");
    assert_eq!(
        serde_json::from_value::<RenderState>(serde_json::json!({
            "style": serde_json::to_value(Style::default()).unwrap(),
            "charsets": ["Ascii", "Ascii", "Ascii", "Ascii"],
            "gl": 3,
        }))
        .expect("the last slot reads back")
        .gl,
        3
    );
}
