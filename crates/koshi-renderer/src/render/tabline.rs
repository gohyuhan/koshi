//! The tab bar: which tabs fit the row, scroll arrows, and the
//! session/mode block on the right.

use super::*;

#[cfg(test)]
mod tests;

/// Draw the tabline: the whole row is filled with the theme's bar background
/// (black by default), then the session name plus the `[v…]` version badge on
/// the left and the mode tag on the right are shown whole as colored text over
/// that fill; only the tab list between them carries its own block
/// backgrounds — each tab a hint-bar-style ribbon on its own stop of the
/// theme's chrome ramp (light-purple → light-blue by default). Tabs that don't
/// fit are dropped whole, and a `◀` or `▶` marks the side they went off.
///
/// The block widths and per-tab cell spans come from [`tabline_layout`], the
/// same solve [`crate::hit_test()`] reads, so the tab a click lands on is the tab
/// that was drawn there.
pub(super) fn draw_tabline(
    frame: FrameLayout<'_>,
    theme: &Theme,
    area: RatatuiRect,
    buf: &mut Buffer,
) {
    // The row is koshi-owned chrome: reset it first so no letterbox fill or
    // stale cell survives, then fill it with the theme's bar background. Text
    // painted after this sets only its foreground, so the fill shows through
    // as the row's background.
    Clear.render(area, buf);
    buf.set_style(area, bar_style(theme));

    let layout = tabline_layout(frame, area);

    // Right block: it owns the right edge whole.
    let right = right_block(frame, theme);
    set_line_clipped(
        buf,
        layout.right_x,
        area.y,
        &right,
        area.right() - layout.right_x,
    );

    // Left block: the session name and version badge. The same `room` the
    // solve used, so draw and layout agree on whether the badge is there.
    let session = session_line(frame, theme, layout.right_x.saturating_sub(area.x));
    set_line_clipped(buf, area.x, area.y, &session, layout.session_width);

    // Tab ribbons in the windowed middle, each on its own ramp stop.
    for &(meta_index, x, width) in &layout.tabs {
        let tab = tab_line(frame, theme, meta_index);
        set_line_clipped(buf, x, area.y, &tab, width);
    }
    // Clickable scroll arrows mark tabs hidden off each side, and scroll the
    // strip one tab that way when clicked.
    if let Some((x, _)) = layout.left_arrow {
        let arrow = Line::from(Span::styled("◀", scroll_arrow_style(theme)));
        set_line_clipped(buf, x, area.y, &arrow, TABLINE_ARROW_WIDTH);
    }
    if let Some((x, _)) = layout.right_arrow {
        let arrow = Line::from(Span::styled("▶", scroll_arrow_style(theme)));
        set_line_clipped(buf, x, area.y, &arrow, TABLINE_ARROW_WIDTH);
    }
}

/// The one-cell width a tabline scroll arrow reserves and occupies.
pub(crate) const TABLINE_ARROW_WIDTH: u16 = 1;

/// The tabline's solved geometry for one frame: the two anchored block widths,
/// the windowed run of visible tabs, and the scroll arrows framing it.
///
/// [`draw_tabline`] paints from it and [`crate::hit_test()`] maps a click to a tab
/// or arrow with it, so the drawn positions and the hit-tested ones cannot
/// drift apart — they are the same solve.
pub(crate) struct TablineLayout {
    /// Cells the left session block occupies, measured from `area.x`.
    pub session_width: u16,
    /// The x where the right block (scroll + mode tag) starts.
    pub right_x: u16,
    /// The metadata index of the first tab in the visible window.
    pub first_visible: usize,
    /// `(tab metadata index, x, width)` for each tab in the window, left to
    /// right. The tab occupies the half-open column span `[x, x + width)`.
    pub tabs: Vec<(usize, u16, u16)>,
    /// The left scroll arrow when tabs are hidden off the left: its cell `x`
    /// and the first-visible index a click on it scrolls to.
    pub left_arrow: Option<(u16, usize)>,
    /// The right scroll arrow when tabs are hidden off the right: its cell `x`
    /// and the first-visible index a click on it scrolls to.
    pub right_arrow: Option<(u16, usize)>,
}

/// Solve the tabline's block widths, its windowed run of tabs, and its scroll
/// arrows for `area`.
///
/// The right block anchors the right edge and the session block the left. If
/// every tab fits in the gap between them, all are shown from index 0 with no
/// arrows. Otherwise the strip scrolls: the window starts at the client's
/// [`tabline_offset`](crate::snapshot::ViewerChrome::tabline_offset) when it
/// is peeking, or — following the active tab — at the smallest index that keeps
/// the active tab on screen. A one-cell arrow is reserved on each side while
/// scrolled and drawn on whichever side still hides tabs.
pub(crate) fn tabline_layout(frame: FrameLayout<'_>, area: RatatuiRect) -> TablineLayout {
    let right_width = text_width(&right_block_text(frame));
    let right_x = area.right().saturating_sub(right_width).max(area.x);
    let room = right_x.saturating_sub(area.x);
    let session_width = session_texts(frame, room).width.min(room);
    let strip_start = area.x.saturating_add(session_width).saturating_add(1);

    let count = frame.session.tabs_metadata.len();
    let widths: Vec<u16> = (0..count)
        .map(|i| {
            let (index, name) = tab_texts(frame, i);
            text_width(&index).saturating_add(text_width(&name))
        })
        .collect();

    let empty = |first_visible| TablineLayout {
        session_width,
        right_x,
        first_visible,
        tabs: Vec::new(),
        left_arrow: None,
        right_arrow: None,
    };
    if count == 0 || strip_start >= right_x {
        return empty(0);
    }

    // Everything fits from the first tab: show them all, no scrolling.
    let full = pack_tabs(&widths, 0, strip_start, right_x);
    if full.len() == count {
        return TablineLayout {
            session_width,
            right_x,
            first_visible: 0,
            tabs: full,
            left_arrow: None,
            right_arrow: None,
        };
    }

    // Scrolled: reserve one arrow cell on each side. A reserved-but-undrawn
    // cell (no tabs hidden that side) is a harmless one-cell gap.
    let lo = strip_start.saturating_add(TABLINE_ARROW_WIDTH);
    let hi = right_x.saturating_sub(TABLINE_ARROW_WIDTH);
    if lo >= hi {
        return empty(0);
    }

    let active = frame
        .session
        .tabs_metadata
        .iter()
        .position(|meta| meta.active)
        .unwrap_or(0);
    let first_visible = match frame.viewer.tabline_offset {
        Some(i) => i.min(count - 1),
        None => reveal_active(&widths, active, lo, hi),
    };

    let tabs = pack_tabs(&widths, first_visible, lo, hi);
    let after_window = first_visible + tabs.len();
    let left_arrow = (first_visible > 0).then(|| (strip_start, first_visible - 1));
    let right_arrow =
        (after_window < count).then(|| (right_x - TABLINE_ARROW_WIDTH, first_visible + 1));

    TablineLayout {
        session_width,
        right_x,
        first_visible,
        tabs,
        left_arrow,
        right_arrow,
    }
}

/// Place tabs from index `first` into the half-open column range `[lo, hi)`
/// with a one-cell gap between them, stopping at the first that would not fit.
/// Returns `(metadata index, x, width)` for each placed tab.
fn pack_tabs(widths: &[u16], first: usize, lo: u16, hi: u16) -> Vec<(usize, u16, u16)> {
    let mut tabs = Vec::new();
    let mut x = lo;
    for (i, &width) in widths.iter().enumerate().skip(first) {
        if u32::from(x) + u32::from(width) > u32::from(hi) {
            break;
        }
        tabs.push((i, x, width));
        x = x.saturating_add(width).saturating_add(1);
    }
    tabs
}

/// The smallest first-visible index that keeps tab `active` on screen when
/// packing into `[lo, hi)`: `0` if `active` already fits from the left,
/// otherwise the leftmost start that still shows `active` at the right edge.
fn reveal_active(widths: &[u16], active: usize, lo: u16, hi: u16) -> usize {
    let shows_active = |first: usize| {
        pack_tabs(widths, first, lo, hi)
            .iter()
            .any(|&(i, _, _)| i == active)
    };
    if shows_active(0) {
        return 0;
    }
    let mut start = active;
    while start > 0 && shows_active(start - 1) {
        start -= 1;
    }
    start
}

/// The cells `text` occupies when drawn. A span's width counts the graphemes
/// in its text and never looks at its style, so measuring an unstyled span
/// gives the same answer the drawn one does — which is what lets the solve
/// below run without any colors.
///
/// Text wider than a `u16` saturates rather than wrapping: no terminal is
/// 65535 cells across, so anything at the cap is already "wider than the row"
/// and every comparison below treats it that way.
fn text_width(text: &str) -> u16 {
    u16::try_from(Span::raw(text).width()).unwrap_or(u16::MAX)
}

/// The tabline's right-anchored block text: the mode tag. Each pane's scroll
/// position lives in its own bottom border (see [`draw_panes`]), not here.
fn right_block_text(frame: FrameLayout<'_>) -> String {
    format!(" {} ", mode_tags(frame.client))
}

/// The right-anchored block, colored for drawing.
fn right_block(frame: FrameLayout<'_>, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(right_block_text(frame), mode_style(theme)))
}

/// koshi's own version, shown as the `[v…]` badge beside the session name.
/// Every workspace crate inherits the one workspace version, so the renderer's
/// own `CARGO_PKG_VERSION` is the running binary's version.
const KOSHI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The tabline's left-anchored block text: the session name, then the `[v…]`
/// badge naming the koshi version that is running — ` my-session [v0.1.0] `.
///
/// `room` is the space the block has before the right-anchored mode tag. When
/// both parts do not fit in it the badge is dropped whole, the way a tab that
/// does not fit is dropped rather than clipped — a 16-cell row shows ` s `,
/// never the half-written ` s [v0.1.0`.
fn session_texts(frame: FrameLayout<'_>, room: u16) -> SessionBlock {
    let name = format!(" {} ", frame.session.name);
    let badge = format!("[v{KOSHI_VERSION}] ");
    let name_width = text_width(&name);
    let badge_width = text_width(&badge);
    if name_width.saturating_add(badge_width) <= room {
        SessionBlock {
            width: name_width.saturating_add(badge_width),
            name,
            badge: Some(badge),
        }
    } else {
        SessionBlock {
            width: name_width,
            name,
            badge: None,
        }
    }
}

/// The left block's text and the cells it occupies, measured once.
///
/// The solve and the draw both need this, and measuring text means walking it
/// grapheme by grapheme — so the width the fit decision already computed is
/// carried out rather than recomputed. `tabline_layout` runs on every pointer
/// move, so the walks it does are worth counting.
struct SessionBlock {
    /// The session name, padded — always drawn.
    name: String,
    /// The version badge, present only when it fit whole beside the name.
    badge: Option<String>,
    /// Cells `name` plus `badge` occupy together.
    width: u16,
}

/// The left-anchored block, colored for drawing.
fn session_line(frame: FrameLayout<'_>, theme: &Theme, room: u16) -> Line<'static> {
    let block = session_texts(frame, room);
    let name = Span::styled(block.name, session_style(theme));
    match block.badge {
        Some(badge) => Line::from(vec![name, Span::styled(badge, version_style(theme))]),
        None => Line::from(name),
    }
}

/// One tab's two text blocks at metadata index `meta_index`: its `#N` block
/// and its name block.
fn tab_texts(frame: FrameLayout<'_>, meta_index: usize) -> (String, String) {
    let meta = &frame.session.tabs_metadata[meta_index];
    (format!(" #{} ", meta.index + 1), format!(" {} ", meta.name))
}

/// One tab's two-block ribbon (`#N` block + name block) at metadata index
/// `meta_index`, colored on its own stop of the theme's chrome ramp.
fn tab_line(frame: FrameLayout<'_>, theme: &Theme, meta_index: usize) -> Line<'static> {
    let count = frame.session.tabs_metadata.len();
    let active = frame.session.tabs_metadata[meta_index].active;
    let (index, name) = tab_texts(frame, meta_index);
    Line::from(vec![
        Span::styled(index, tab_index_style(theme, active, meta_index, count)),
        Span::styled(name, tab_name_style(theme, active, meta_index, count)),
    ])
}
