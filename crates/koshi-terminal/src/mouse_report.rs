//! Encode a mouse event into the bytes the program in a pane expects.
//!
//! [`encode_mouse`] produces the mouse report for one event, or [`None`] when
//! the program's current [`MouseTracking`] level does not report this kind of
//! event.
//!
//! Two independent settings shape a report (both tracked in
//! `TerminalModes`):
//!
//! - **Tracking level** ([`MouseTracking`]) — which events are reported at all.
//!   The levels form a ladder: `X10` reports only presses, `Normal` adds
//!   releases and wheel ticks, `ButtonMotion` adds drags, `AnyMotion` adds
//!   buttonless motion.
//! - **Encoding** ([`MouseEncoding`]) — how the button and the 1-based cell are
//!   written: the `Sgr` form (`CSI < b ; x ; y M`), the legacy byte form
//!   (`CSI M` + three `value+32` bytes), its `Utf8` and `Urxvt` variants.
//!
//! The button byte packs the button (left `0`, middle `1`, right `2`, wheel
//! `64`/`65`/`66`/`67`), a `+32` motion bit for a drag or a bare move, and the
//! modifier bits shift `4`, alt `8`, ctrl `16`. In every encoding but `Sgr` a
//! release reports button `3` in place of the button that came up; `Sgr`
//! keeps the button and marks the release with a trailing `m` instead of `M`.

use koshi_core::key::ModFlags;
use koshi_core::mouse::{is_mouse_kind_reported, MouseButton, MouseKind, ScrollDirection};

use crate::state::{MouseEncoding, MouseTracking};

#[cfg(test)]
mod tests;

/// The bytes a program expects for one mouse event at a 1-based pane-local
/// cell (`column_index`, `row_index`), or [`None`] when `mouse_tracking` does not
/// report this event kind.
///
/// A left press at the top-left cell under SGR encoding is `CSI < 0 ; 1 ; 1 M`
/// (`\x1b[<0;1;1M`); the same release is `\x1b[<0;1;1m`.
#[must_use]
pub fn encode_mouse(
    mouse_kind: MouseKind,
    modifier_flags: ModFlags,
    column_index: u16,
    row_index: u16,
    mouse_tracking: MouseTracking,
    mouse_encoding: MouseEncoding,
) -> Option<Vec<u8>> {
    if !is_mouse_kind_reported(mouse_tracking, mouse_kind) {
        return None;
    }
    // X10 compatibility mode (`?9`) carries only the button in its report; the
    // modifier bits enter at normal tracking (`?1000`) and beyond.
    let mouse_modifier_bits = if mouse_tracking == MouseTracking::X10 {
        0
    } else {
        compute_mouse_modifier_bits(modifier_flags)
    };
    let is_mouse_release = matches!(mouse_kind, MouseKind::Release(_));
    let should_drop_button_on_release = mouse_encoding != MouseEncoding::Sgr;
    let mouse_button_code_with_modifiers =
        compute_mouse_button_code(mouse_kind, should_drop_button_on_release) + mouse_modifier_bits;
    Some(match mouse_encoding {
        MouseEncoding::Sgr => encode_sgr_mouse_report(
            mouse_button_code_with_modifiers,
            column_index,
            row_index,
            is_mouse_release,
        ),
        MouseEncoding::Default => {
            encode_legacy_mouse_report(mouse_button_code_with_modifiers, column_index, row_index)
        }
        MouseEncoding::Utf8 => {
            encode_utf8_mouse_report(mouse_button_code_with_modifiers, column_index, row_index)
        }
        MouseEncoding::Urxvt => {
            encode_urxvt_mouse_report(mouse_button_code_with_modifiers, column_index, row_index)
        }
    })
}

/// The button code before modifiers: a press is its button number, a drag is
/// its button number plus `32`, a bare move is `35`, a scroll is its wheel
/// number, and a release is `3` when `should_drop_button_on_release` is true
/// or its button number when false.
fn compute_mouse_button_code(mouse_kind: MouseKind, should_drop_button_on_release: bool) -> u16 {
    const MOTION_FLAG_BIT: u16 = 32;
    match mouse_kind {
        MouseKind::Press(mouse_button) => compute_mouse_button_number(mouse_button),
        MouseKind::Drag(mouse_button) => {
            compute_mouse_button_number(mouse_button) + MOTION_FLAG_BIT
        }
        MouseKind::Motion => 3 + MOTION_FLAG_BIT,
        MouseKind::Release(mouse_button) => {
            if should_drop_button_on_release {
                3
            } else {
                compute_mouse_button_number(mouse_button)
            }
        }
        MouseKind::Scroll(scroll_direction) => compute_mouse_wheel_number(scroll_direction),
    }
}

/// Left `0`, middle `1`, right `2`.
fn compute_mouse_button_number(mouse_button: MouseButton) -> u16 {
    match mouse_button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Wheel up `64`, down `65`, left `66`, right `67`.
fn compute_mouse_wheel_number(scroll_direction: ScrollDirection) -> u16 {
    match scroll_direction {
        ScrollDirection::Up => 64,
        ScrollDirection::Down => 65,
        ScrollDirection::Left => 66,
        ScrollDirection::Right => 67,
    }
}

/// Shift `4`, alt `8`, ctrl `16`, summed. Super adds nothing.
fn compute_mouse_modifier_bits(modifier_flags: ModFlags) -> u16 {
    let mut mouse_modifier_bits = 0;
    if modifier_flags.has_all_modifiers(ModFlags::SHIFT) {
        mouse_modifier_bits += 4;
    }
    if modifier_flags.has_all_modifiers(ModFlags::ALT) {
        mouse_modifier_bits += 8;
    }
    if modifier_flags.has_all_modifiers(ModFlags::CTRL) {
        mouse_modifier_bits += 16;
    }
    mouse_modifier_bits
}

/// `CSI < mouse_button_code ; column_index ; row_index M` (or a trailing `m` for a release).
fn encode_sgr_mouse_report(
    mouse_button_code: u16,
    column_index: u16,
    row_index: u16,
    is_mouse_release: bool,
) -> Vec<u8> {
    let mouse_release_terminator = if is_mouse_release { 'm' } else { 'M' };
    format!("\x1b[<{mouse_button_code};{column_index};{row_index}{mouse_release_terminator}")
        .into_bytes()
}

/// `CSI M` then `mouse_button_code+32`, `column_index+32`, `row_index+32` as one byte each, saturating at
/// `255`.
fn encode_legacy_mouse_report(
    mouse_button_code: u16,
    column_index: u16,
    row_index: u16,
) -> Vec<u8> {
    vec![
        0x1b,
        b'[',
        b'M',
        compute_legacy_mouse_report_byte(mouse_button_code),
        compute_legacy_mouse_report_byte(column_index),
        compute_legacy_mouse_report_byte(row_index),
    ]
}

/// `CSI M` then `mouse_button_code+32`, `column_index+32`, `row_index+32`, each written as UTF-8: one byte
/// below `128`, two bytes up to `2047`, three up to `65535`, four above.
fn encode_utf8_mouse_report(mouse_button_code: u16, column_index: u16, row_index: u16) -> Vec<u8> {
    let mut encoded_mouse_bytes = vec![0x1b, b'[', b'M'];
    for mouse_report_number in [mouse_button_code, column_index, row_index] {
        append_utf8_character_encoding(
            &mut encoded_mouse_bytes,
            u32::from(mouse_report_number) + 32,
        );
    }
    encoded_mouse_bytes
}

/// `CSI (mouse_button_code+32) ; column_index ; row_index M`, every value in decimal.
fn encode_urxvt_mouse_report(mouse_button_code: u16, column_index: u16, row_index: u16) -> Vec<u8> {
    format!(
        "\x1b[{};{column_index};{row_index}M",
        mouse_button_code + 32
    )
    .into_bytes()
}

/// `mouse_report_number + 32`, summed in `u32`, capped at `255`, then narrowed to
/// one byte.
fn compute_legacy_mouse_report_byte(mouse_report_number: u16) -> u8 {
    (u32::from(mouse_report_number) + 32).min(255) as u8
}

/// Append `unicode_code_point` as UTF-8. A code point that is not a valid `char` — a
/// surrogate in `0xD800..=0xDFFF`, or above `0x10FFFF` — is written as `?`.
fn append_utf8_character_encoding(mouse_report_bytes: &mut Vec<u8>, unicode_code_point: u32) {
    let mouse_report_character = char::from_u32(unicode_code_point).unwrap_or('?');
    let mut unicode_character_bytes = [0; 4];
    mouse_report_bytes.extend_from_slice(
        mouse_report_character
            .encode_utf8(&mut unicode_character_bytes)
            .as_bytes(),
    );
}
