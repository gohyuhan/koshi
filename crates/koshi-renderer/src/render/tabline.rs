//! The tab bar: the session block on the left, the mode tag on the right, and
//! between them the tabs that fit the row with their scroll arrows.

use super::*;

use crate::region::TablineInputs;
use crate::snapshot::TabMeta;

#[cfg(test)]
mod tests;

/// Draw the tabline from `inputs` — see [`TablineInputs`] — in `theme`'s
/// colors. `area` is the row to paint. `buf` is the buffer painted into.
///
/// The whole row is filled with the theme's bar background (black by default).
/// The session name with the `[v…]` version badge sits on the left and the mode
/// tag on the right, painted as colored text over that fill. Only the tab list
/// between them carries block backgrounds — each tab is a two-block ribbon on
/// its own stop of the theme's chrome ramp (light-purple → light-blue by
/// default). A tab that does not fit is dropped whole, and a `◀` or `▶` marks
/// the side it went off.
///
/// The block widths and per-tab cell spans come from [`tabline_layout`], the
/// same solve [`crate::hit_test()`] reads. A row outside `buf` paints nothing,
/// and a zero-width or zero-height `area` paints nothing.
pub(super) fn draw_tabline(
    inputs: TablineInputs<'_>,
    theme: &Theme,
    area: RatatuiRect,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Reset every cell of the row, then fill it with the theme's bar
    // background. The session name, badge, mode tag, and arrows set only a
    // foreground, so the fill stays their background; an inactive tab's two
    // blocks set their own background over it.
    Clear.render(area, buf);
    buf.set_style(area, bar_style(theme));

    let layout = tabline_layout(inputs, area);

    // Right block: it owns the right edge whole.
    let right = right_block(inputs, theme);
    set_line_clipped(
        buf,
        layout.right_x,
        area.y,
        &right,
        area.right() - layout.right_x,
    );

    // Left block: the session name and version badge, measured with the same
    // `room` the solve used.
    let session = session_line(
        inputs.session_name,
        theme,
        layout.right_x.saturating_sub(area.x),
    );
    set_line_clipped(buf, area.x, area.y, &session, layout.session_width);

    // Tab ribbons in the windowed middle, each on its own ramp stop.
    for &(meta_index, x, width) in &layout.tabs {
        let tab = tab_line(inputs.tabs, theme, meta_index);
        set_line_clipped(buf, x, area.y, &tab, width);
    }
    // A `◀`/`▶` on each side that still hides tabs.
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

/// The tabline's solved geometry for one frame: where the two anchored blocks
/// sit, the windowed run of visible tabs, and the scroll arrows framing it.
///
/// [`draw_tabline`] paints from it, and [`crate::hit_test()`] maps a click to a
/// tab or arrow with it. Both read the same solve.
pub(crate) struct TablineLayout {
    /// Cells the left session block occupies, measured from `area.x`.
    pub session_width: u16,
    /// The x where the right block (scroll + mode tag) starts.
    pub right_x: u16,
    /// The metadata index the visible window starts at. When no tab fits, it
    /// is still the index the window starts from.
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
/// arrows. Otherwise the strip scrolls, a one-cell arrow is reserved on each
/// side and drawn on whichever side still hides tabs, and the window starts at:
///
/// - the client's
///   [`tabline_offset`](crate::snapshot::ViewerChrome::tabline_offset) when it
///   is peeking, clamped to the last tab;
/// - otherwise the smallest index that keeps the active tab on screen, which is
///   the active tab's own index when that tab is wider than the window — a
///   window holding no tab at all.
///
/// The first tab marked active is the one followed, and index 0 when none is
/// marked. A row leaving no gap between the two blocks yields no tabs and no
/// arrows.
pub(crate) fn tabline_layout(inputs: TablineInputs<'_>, area: RatatuiRect) -> TablineLayout {
    let right_width = text_width(&right_block_text(inputs));
    let right_x = area.right().saturating_sub(right_width).max(area.x);
    let room = right_x.saturating_sub(area.x);
    let session_width = session_texts(inputs.session_name, room).width.min(room);
    let strip_start = area.x.saturating_add(session_width).saturating_add(1);

    let count = inputs.tabs.len();
    let no_tabs = || TablineLayout {
        session_width,
        right_x,
        first_visible: 0,
        tabs: Vec::new(),
        left_arrow: None,
        right_arrow: None,
    };
    if count == 0 || strip_start >= right_x {
        return no_tabs();
    }

    let widths: Vec<u16> = (0..count)
        .map(|i| {
            let (index, name) = tab_texts(inputs.tabs, i);
            text_width(&index).saturating_add(text_width(&name))
        })
        .collect();

    // Everything fits from the first tab: show them all, no scrolling.
    let unscrolled = pack_tabs(&widths, 0, strip_start, right_x);
    if unscrolled.len() == count {
        return TablineLayout {
            session_width,
            right_x,
            first_visible: 0,
            tabs: unscrolled,
            left_arrow: None,
            right_arrow: None,
        };
    }

    // Scrolled: reserve one arrow cell on each side. A reserved cell that
    // draws no arrow (nothing hidden that side) stays a one-cell gap.
    let lo = strip_start.saturating_add(TABLINE_ARROW_WIDTH);
    let hi = right_x.saturating_sub(TABLINE_ARROW_WIDTH);
    if lo >= hi {
        return no_tabs();
    }

    let active = inputs.tabs.iter().position(|meta| meta.active).unwrap_or(0);
    let first_visible = match inputs.tabline_offset {
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
/// packing into `[lo, hi)`. `0` when `active` already fits from the left, and
/// `active` itself when that tab is wider than `[lo, hi)` — a window that shows
/// no tab at all.
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

/// The tabline's right-anchored block text: the tag [`mode_tags`] composes,
/// with a space each side. A plain client gives ` BASE `; one that is locked,
/// selecting, and dialing the session again gives
/// ` RECONNECTING (attempt 4, retry in 8s) · LOCK · SELECT `.
fn right_block_text(inputs: TablineInputs<'_>) -> String {
    format!(
        " {} ",
        mode_tags(inputs.lock_mode, inputs.mouse_select, inputs.reconnecting,)
    )
}

/// The right-anchored block, colored for drawing.
fn right_block(inputs: TablineInputs<'_>, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(right_block_text(inputs), mode_style(theme)))
}

/// koshi's own version, shown as the `[v…]` badge beside the session name.
/// This crate's `CARGO_PKG_VERSION`, which the workspace sets for every koshi
/// crate.
const KOSHI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `[v…] ` badge text the tabline paints, trailing space included.
pub(crate) fn version_badge() -> String {
    format!("[v{KOSHI_VERSION}] ")
}

/// The tabline's left-anchored block text: the session name with a space each
/// side, then the `[v…] ` badge from [`version_badge`]. Session `my-session`
/// gives ` my-session ` followed by that badge.
///
/// `room` is the cells the block has before the right-anchored mode tag. When
/// both parts do not fit in `room`, the badge is dropped whole: a 16-cell row
/// ending in the 6-cell ` BASE ` tag shows ` s ` alone, never a badge cut off
/// part-way. The returned width is not clipped to `room`; a name wider than
/// `room` carries its own full width out.
fn session_texts(session_name: &str, room: u16) -> SessionBlock {
    let name = format!(" {} ", session_name);
    let badge = version_badge();
    let name_width = text_width(&name);
    let badge_width = text_width(&badge);
    let both_width = name_width.saturating_add(badge_width);
    if both_width <= room {
        SessionBlock {
            width: both_width,
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

/// The left block's text and the cells it occupies, measured once by
/// [`session_texts`].
struct SessionBlock {
    /// The session name with a space each side. Always drawn.
    name: String,
    /// The version badge, present only when it fit whole beside the name.
    badge: Option<String>,
    /// Cells `name` and `badge` occupy together, or `name`'s alone when there
    /// is no badge.
    width: u16,
}

/// The left-anchored block, colored for drawing. `room` is the cells before the
/// mode tag, as in [`session_texts`], and decides whether the badge is there.
fn session_line(session_name: &str, theme: &Theme, room: u16) -> Line<'static> {
    let block = session_texts(session_name, room);
    let name = Span::styled(block.name, session_style(theme));
    match block.badge {
        Some(badge) => Line::from(vec![name, Span::styled(badge, version_style(theme))]),
        None => Line::from(name),
    }
}

/// One tab's two text blocks at metadata index `meta_index`: ` #N `, where `N`
/// is the tab's own `index` field plus one, and its name with a space each side.
/// A tab with `index: 0` and `name: "shell"` gives `(" #1 ", " shell ")`.
///
/// # Panics
///
/// Panics when `meta_index` is not an index of `tabs`.
fn tab_texts(tabs: &[TabMeta], meta_index: usize) -> (String, String) {
    let meta = &tabs[meta_index];
    (format!(" #{} ", meta.index + 1), format!(" {} ", meta.name))
}

/// One tab's two-block ribbon (`#N` block + name block) at metadata index
/// `meta_index`, colored on its own stop of the theme's chrome ramp.
///
/// # Panics
///
/// Panics when `meta_index` is not an index of `tabs`.
fn tab_line(tabs: &[TabMeta], theme: &Theme, meta_index: usize) -> Line<'static> {
    let count = tabs.len();
    let active = tabs[meta_index].active;
    let (index, name) = tab_texts(tabs, meta_index);
    Line::from(vec![
        Span::styled(index, tab_index_style(theme, active, meta_index, count)),
        Span::styled(name, tab_name_style(theme, active, meta_index, count)),
    ])
}
