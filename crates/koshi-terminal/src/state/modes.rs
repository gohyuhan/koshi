//! Terminal mode flags and the mouse tracking/encoding levels the renderer and
//! input layers consult.

use serde::{Deserialize, Serialize};

/// Which mouse events a pane's program asked to be reported. The terminal
/// engine sets it from the DEC private modes `?9`/`?1000`/`?1002`/`?1003`.
/// Independent of [`MouseEncoding`]. Full documentation on
/// [`koshi_core::mouse::MouseTracking`].
pub use koshi_core::mouse::MouseTracking;

/// How a mouse report's coordinate bytes are encoded, set via the DEC private
/// modes `?1005`/`?1006`/`?1015`. Orthogonal to [`MouseTracking`]: an app sets a
/// tracking level and an encoding independently (e.g. `?1000h` then `?1006h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MouseEncoding {
    /// Legacy X10 single-byte coordinates (default).
    #[default]
    Default,
    /// `?1005` UTF-8 extended coordinates.
    Utf8,
    /// `?1006` SGR form (`CSI < … M`/`m`).
    Sgr,
    /// `?1015` urxvt decimal form.
    Urxvt,
}

/// The shape the cursor is drawn as, set by DECSCUSR (`CSI Ps SP q`). An
/// editor switches it to tell its modes apart: vim draws a [`Block`][Self::Block]
/// while it is in normal mode and a [`Bar`][Self::Bar] while it is inserting.
///
/// There is no `Default` variant. The stored shape is an `Option<CursorShape>`:
/// `None` while the pane has never sent DECSCUSR, and again after
/// `CSI 0 SP q`. With `None`, the renderer keeps the cursor the user
/// configured in their own terminal.
///
/// Blink is stored apart from the shape, in
/// [`TerminalState::is_cursor_blink_enabled`](crate::state::TerminalState::is_cursor_blink_enabled).
/// Two writers set it: DECSCUSR (`1` = blinking block, `2` = steady block) and `?12` (att610). The
/// last one to arrive wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShape {
    /// A box filling the whole cell.
    Block,
    /// A line along the bottom of the cell.
    Underline,
    /// A vertical bar at the cell's left edge — what an editor shows while
    /// inserting text.
    Bar,
}

/// Terminal mode flags the renderer and input/mouse layers consult: autowrap
/// (`?7`), application cursor keys (`?1`), reverse video (`?5`), cursor blink
/// (`?12`), cursor [shape][CursorShape] (DECSCUSR), bracketed paste (`?2004`),
/// the mouse [tracking][MouseTracking] level and [encoding][MouseEncoding]
/// (`?9`/`?1000`/`?1002`/`?1003` and `?1005`/`?1006`/`?1015`),
/// alternate-scroll (`?1007`), left/right margin mode (`?69`), Sixel
/// scrolling (`?80`), Sixel register scope (`?1070`), and Sixel cursor
/// movement (`?8452`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TerminalModes {
    /// `?2004` — bracketed paste: the input layer wraps pasted text in
    /// `ESC[200~`…`ESC[201~`.
    pub(in crate::state) is_bracketed_paste_enabled: bool,
    /// Which mouse events are reported; see [`MouseTracking`].
    pub(in crate::state) mouse_tracking: MouseTracking,
    /// How mouse reports are encoded; see [`MouseEncoding`].
    pub(in crate::state) mouse_encoding: MouseEncoding,
    /// `?1007` — alternate scroll: on the alternate screen, the mouse layer
    /// sends cursor arrow keys for wheel motion.
    pub(in crate::state) is_alternate_scroll_enabled: bool,
    /// `?7` (DECAWM) — autowrap. On (the default), a glyph printed into the
    /// effective right bound parks the cursor there and the next glyph wraps to
    /// a new line. Off, the next glyph overwrites the bound in place.
    pub(in crate::state) is_autowrap_enabled: bool,
    /// `?1` (DECCKM) — application cursor keys: the input layer sends `ESC O A`
    /// for the arrow keys; off, it sends `ESC [ A`.
    pub(in crate::state) is_application_cursor_keys_enabled: bool,
    /// `?69` (DECLRMM) — enables DECSLRM left/right margin setting and use.
    pub(in crate::state) is_left_right_margin_mode_enabled: bool,
    /// `?5` (DECSCNM) — reverse video: the renderer swaps foreground and
    /// background across the whole screen.
    pub(in crate::state) is_reverse_video_enabled: bool,
    /// `?12` (att610) — cursor blink: the renderer blinks the cursor cell.
    /// Written by `?12` and by DECSCUSR, whose value carries both shape and
    /// blink; the last of the two to arrive wins.
    pub(in crate::state) is_cursor_blink_enabled: bool,
    /// DECSCUSR (`CSI Ps SP q`) — the shape the cursor is drawn as, or `None`
    /// while the pane has asked for no shape (at startup, and again after
    /// `CSI 0 SP q`). With `None`, the renderer keeps the user's own terminal
    /// cursor; see [`CursorShape`].
    pub(in crate::state) cursor_shape: Option<CursorShape>,
    /// `?80` — Sixel scrolling: a graphic at the bottom can scroll the image
    /// area into the primary screen's scroll region.
    pub(in crate::state) is_sixel_scrolling_enabled: bool,
    /// `?1070` — use private Sixel color registers for each graphic.
    pub(in crate::state) is_sixel_private_color_registers_enabled: bool,
    /// `?8452` — leave the cursor to the right of a Sixel graphic.
    pub(in crate::state) is_sixel_cursor_right_enabled: bool,
}

impl Default for TerminalModes {
    fn default() -> Self {
        TerminalModes {
            is_bracketed_paste_enabled: false,
            mouse_tracking: MouseTracking::Off,
            mouse_encoding: MouseEncoding::Default,
            is_alternate_scroll_enabled: false,
            is_autowrap_enabled: true,
            is_application_cursor_keys_enabled: false,
            is_left_right_margin_mode_enabled: false,
            is_reverse_video_enabled: false,
            is_cursor_blink_enabled: false,
            cursor_shape: None,
            is_sixel_scrolling_enabled: true,
            is_sixel_private_color_registers_enabled: true,
            is_sixel_cursor_right_enabled: false,
        }
    }
}

#[cfg(test)]
mod tests;
