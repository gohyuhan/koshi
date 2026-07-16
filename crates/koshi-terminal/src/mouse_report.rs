//! Encode a mouse event into the bytes the program in a pane expects.
//!
//! The reverse of decoding a host mouse event: once koshi decides a mouse event
//! belongs to the program running in a pane (not to koshi's own chrome), it must
//! hand that program a mouse *report* in the exact form the program asked for.
//! [`encode_mouse`] produces those bytes, or [`None`] when the program's current
//! [`MouseTracking`] level does not report this kind of event.
//!
//! Two independent settings shape a report (both tracked in
//! [`TerminalModes`](crate::state::TerminalModes)):
//!
//! - **Tracking level** ([`MouseTracking`]) — which events are reported at all.
//!   The levels form a ladder: `X10` reports only presses, `Normal` adds
//!   releases and wheel ticks, `ButtonMotion` adds drags, `AnyMotion` adds
//!   buttonless motion.
//! - **Encoding** ([`MouseEncoding`]) — how the button and the 1-based cell are
//!   written: the modern `Sgr` form (`CSI < b ; x ; y M`), the legacy byte form
//!   (`CSI M` + three `value+32` bytes), its `Utf8` and `Urxvt` variants.
//!
//! The button byte packs the button (left `0`, middle `1`, right `2`, wheel
//! `64`/`65`/`66`/`67`), a `+32` motion bit for a drag or a bare move, and the
//! modifier bits shift `4`, alt `8`, ctrl `16`. In every encoding but `Sgr` a
//! release cannot say which button came up, so it reports button `3`; `Sgr`
//! keeps the button and marks the release with a trailing `m` instead of `M`.

use koshi_core::key::ModFlags;
use koshi_core::mouse::{MouseButton, MouseKind, ScrollDirection};

use crate::state::{MouseEncoding, MouseTracking};

#[cfg(test)]
mod tests;

/// The bytes a program expects for one mouse event at 1-based pane-local cell
/// (`col`, `row`), or [`None`] when `tracking` does not report this event kind.
///
/// A left press at the top-left cell under SGR encoding is `CSI < 0 ; 1 ; 1 M`
/// (`\x1b[<0;1;1M`); the same release is `\x1b[<0;1;1m`.
#[must_use]
pub fn encode_mouse(
    kind: MouseKind,
    mods: ModFlags,
    col: u16,
    row: u16,
    tracking: MouseTracking,
    encoding: MouseEncoding,
) -> Option<Vec<u8>> {
    if !reports(tracking, kind) {
        return None;
    }
    // X10 compatibility mode (`?9`) carries only the button in its report; the
    // modifier bits enter at normal tracking (`?1000`) and beyond.
    let modifiers = if tracking == MouseTracking::X10 {
        0
    } else {
        mod_bits(mods)
    };
    let released = matches!(kind, MouseKind::Release(_));
    Some(match encoding {
        MouseEncoding::Sgr => encode_sgr(button_code(kind, false) + modifiers, col, row, released),
        MouseEncoding::Default => encode_legacy(button_code(kind, true) + modifiers, col, row),
        MouseEncoding::Utf8 => encode_utf8(button_code(kind, true) + modifiers, col, row),
        MouseEncoding::Urxvt => encode_urxvt(button_code(kind, true) + modifiers, col, row),
    })
}

/// Whether a program at `tracking` is told about a `kind` of event. The ladder:
/// every level but `Off` reports a press, `Normal` and up add releases,
/// `ButtonMotion` and up add drags, only `AnyMotion` adds buttonless motion. A
/// wheel tick reports from `Normal` up — `X10` predates the wheel and reports
/// only presses.
///
/// The forward path calls this to skip the frame rebuild for an event the pane's
/// program does not want — a bare move over a pane not in `AnyMotion` costs
/// nothing.
#[must_use]
pub fn reports(tracking: MouseTracking, kind: MouseKind) -> bool {
    match kind {
        MouseKind::Press(_) => tracking != MouseTracking::Off,
        MouseKind::Release(_) | MouseKind::Scroll(_) => matches!(
            tracking,
            MouseTracking::Normal | MouseTracking::ButtonMotion | MouseTracking::AnyMotion
        ),
        MouseKind::Drag(_) => matches!(
            tracking,
            MouseTracking::ButtonMotion | MouseTracking::AnyMotion
        ),
        MouseKind::Motion => tracking == MouseTracking::AnyMotion,
    }
}

/// The button code before modifiers: the button number, plus `32` for a drag or
/// a bare move, plus the wheel base for a scroll. A release loses the button
/// (reports `3`) when `release_loses_button` — every encoding but `Sgr` loses it.
fn button_code(kind: MouseKind, release_loses_button: bool) -> u16 {
    const MOTION: u16 = 32;
    match kind {
        MouseKind::Press(button) => button_number(button),
        MouseKind::Drag(button) => button_number(button) + MOTION,
        MouseKind::Motion => 3 + MOTION,
        MouseKind::Release(button) => {
            if release_loses_button {
                3
            } else {
                button_number(button)
            }
        }
        MouseKind::Scroll(direction) => wheel_number(direction),
    }
}

/// Left `0`, middle `1`, right `2`.
fn button_number(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Wheel up `64`, down `65`, left `66`, right `67`.
fn wheel_number(direction: ScrollDirection) -> u16 {
    match direction {
        ScrollDirection::Up => 64,
        ScrollDirection::Down => 65,
        ScrollDirection::Left => 66,
        ScrollDirection::Right => 67,
    }
}

/// Shift `4`, alt `8`, ctrl `16`, summed. Super has no place in the protocol.
fn mod_bits(mods: ModFlags) -> u16 {
    let mut bits = 0;
    if mods.contains(ModFlags::SHIFT) {
        bits += 4;
    }
    if mods.contains(ModFlags::ALT) {
        bits += 8;
    }
    if mods.contains(ModFlags::CTRL) {
        bits += 16;
    }
    bits
}

/// `CSI < cb ; col ; row M` (or a trailing `m` for a release).
fn encode_sgr(cb: u16, col: u16, row: u16, released: bool) -> Vec<u8> {
    let terminator = if released { 'm' } else { 'M' };
    format!("\x1b[<{cb};{col};{row}{terminator}").into_bytes()
}

/// `CSI M` then three `value+32` bytes, each capped at `255` since a byte cannot
/// hold a larger cell.
fn encode_legacy(cb: u16, col: u16, row: u16) -> Vec<u8> {
    vec![
        0x1b,
        b'[',
        b'M',
        offset_byte(cb),
        offset_byte(col),
        offset_byte(row),
    ]
}

/// Like the legacy form, but each `value+32` is a UTF-8 code point, so a cell
/// past `223` still fits.
fn encode_utf8(cb: u16, col: u16, row: u16) -> Vec<u8> {
    let mut bytes = vec![0x1b, b'[', b'M'];
    for value in [cb, col, row] {
        push_utf8(&mut bytes, u32::from(value) + 32);
    }
    bytes
}

/// `CSI (cb+32) ; col ; row M` — decimal, so no byte cap and no encoding of the
/// cell.
fn encode_urxvt(cb: u16, col: u16, row: u16) -> Vec<u8> {
    format!("\x1b[{};{col};{row}M", cb + 32).into_bytes()
}

/// `value + 32` as a byte, saturating at `255`. Widened to `u32` first so a
/// coordinate near `u16::MAX` cannot overflow the addition.
fn offset_byte(value: u16) -> u8 {
    (u32::from(value) + 32).min(255) as u8
}

/// Append `code_point` as UTF-8, falling back to `?` for a value that is not a
/// valid `char` (unreachable for real cells).
fn push_utf8(bytes: &mut Vec<u8>, code_point: u32) {
    let character = char::from_u32(code_point).unwrap_or('?');
    let mut buffer = [0; 4];
    bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
}
