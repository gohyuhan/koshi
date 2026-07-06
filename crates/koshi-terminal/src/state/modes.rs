//! Terminal mode flags and the mouse tracking/encoding levels the renderer and
//! input layers consult.

/// Which mouse events the running app has asked to be reported, set via the DEC
/// private modes `?9`/`?1000`/`?1002`/`?1003`. The levels form a ladder (each
/// reports strictly more than the one above); an app enables exactly one, and
/// the last enabling sequence wins. Independent of [`MouseEncoding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseTracking {
    /// No mouse reporting (default).
    #[default]
    Off,
    /// `?9` X10 compatibility — button presses only, no releases.
    X10,
    /// `?1000` normal tracking — button presses and releases.
    Normal,
    /// `?1002` button-event tracking — presses, releases, and motion while a
    /// button is held (drag).
    ButtonMotion,
    /// `?1003` any-event tracking — all motion, whether or not a button is held.
    AnyMotion,
}

/// How a mouse report's coordinate bytes are encoded, set via the DEC private
/// modes `?1005`/`?1006`/`?1015`. Orthogonal to [`MouseTracking`]: an app sets a
/// tracking level and an encoding independently (e.g. `?1000h` then `?1006h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// Legacy X10 single-byte coordinates (default).
    #[default]
    Default,
    /// `?1005` UTF-8 extended coordinates.
    Utf8,
    /// `?1006` SGR form (`CSI < … M`/`m`) — the encoding modern apps use.
    Sgr,
    /// `?1015` urxvt decimal form.
    Urxvt,
}

/// Terminal mode flags the renderer and input/mouse layers consult: autowrap
/// (`?7`), application cursor keys (`?1`), reverse video (`?5`), cursor blink
/// (`?12`), bracketed paste (`?2004`), the mouse [tracking][MouseTracking] level
/// and [encoding][MouseEncoding] (`?9`/`?1000`/`?1002`/`?1003` and
/// `?1005`/`?1006`/`?1015`), and alternate-scroll (`?1007`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalModes {
    /// `?2004` — wrap pasted text in `ESC[200~`…`ESC[201~` so the app can tell
    /// typed input from a paste.
    pub bracketed_paste: bool,
    /// Which mouse events are reported; see [`MouseTracking`].
    pub(in crate::state) mouse_tracking: MouseTracking,
    /// How mouse reports are encoded; see [`MouseEncoding`].
    pub(in crate::state) mouse_encoding: MouseEncoding,
    /// `?1007` — on the alternate screen, translate wheel motion into cursor
    /// arrow keys instead of emitting a mouse report.
    pub(in crate::state) alt_scroll: bool,
    /// `?7` (DECAWM) — autowrap. When off, a glyph at the last column overwrites
    /// in place instead of parking to wrap onto a new line. Default on.
    pub(in crate::state) autowrap: bool,
    /// `?1` (DECCKM) — application cursor keys: the input layer sends `ESC O A`
    /// rather than `ESC [ A` for the arrow keys.
    pub(in crate::state) app_cursor_keys: bool,
    /// `?5` (DECSCNM) — reverse video: the renderer swaps foreground and
    /// background across the whole screen.
    pub(in crate::state) reverse_video: bool,
    /// `?12` (att610) — cursor blink: the renderer blinks the cursor cell.
    pub(in crate::state) cursor_blink: bool,
}

impl Default for TerminalModes {
    fn default() -> Self {
        TerminalModes {
            bracketed_paste: false,
            mouse_tracking: MouseTracking::Off,
            mouse_encoding: MouseEncoding::Default,
            alt_scroll: false,
            autowrap: true,
            app_cursor_keys: false,
            reverse_video: false,
            cursor_blink: false,
        }
    }
}
