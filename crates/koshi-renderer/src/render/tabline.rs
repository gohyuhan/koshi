//! The tab bar: which tabs fit the row, scroll arrows, and the
//! session/mode block on the right.

use super::*;

/// Draw the tabline: the session name on the left and the scroll indicator
/// plus mode tag on the right are always shown whole, as colored text on the
/// terminal's own background; only the tab list between them carries
/// backgrounds — each tab a hint-bar-style ribbon on its own stop of the
/// theme's chrome ramp (dark-purple → blue by default). Tabs that don't fit
/// are dropped whole with a trailing `…`.
///
/// The block widths and per-tab cell spans come from [`tabline_layout`], the
/// same solve [`crate::hit_test`] reads, so the tab a click lands on is the tab
/// that was drawn there.
pub(super) fn draw_tabline(snapshot: &RenderSnapshot, area: RatatuiRect, buf: &mut Buffer) {
    // The row is koshi-owned chrome: reset it first so its unused space keeps
    // the terminal's own background — never letterbox fill or stale cells.
    Clear.render(area, buf);

    let layout = tabline_layout(snapshot, area);

    // Right block: it owns the right edge whole.
    let right = right_block(snapshot);
    set_line_clipped(
        buf,
        layout.right_x,
        area.y,
        &right,
        area.right() - layout.right_x,
    );

    // Left block: the session name, always whole (clipped only by the row).
    let session = session_line(snapshot);
    set_line_clipped(buf, area.x, area.y, &session, layout.session_width);

    // Tab ribbons in the windowed middle, each on its own ramp stop.
    for &(meta_index, x, width) in &layout.tabs {
        let tab = tab_line(snapshot, meta_index);
        set_line_clipped(buf, x, area.y, &tab, width);
    }
    // Clickable scroll arrows mark tabs hidden off each side; they replace the
    // old `…` and scroll the strip when clicked.
    if let Some((x, _)) = layout.left_arrow {
        let arrow = Line::from(Span::styled("<", scroll_arrow_style(&snapshot.theme)));
        set_line_clipped(buf, x, area.y, &arrow, TABLINE_ARROW_WIDTH);
    }
    if let Some((x, _)) = layout.right_arrow {
        let arrow = Line::from(Span::styled(">", scroll_arrow_style(&snapshot.theme)));
        set_line_clipped(buf, x, area.y, &arrow, TABLINE_ARROW_WIDTH);
    }
}

/// The one-cell width a tabline scroll arrow reserves and occupies.
pub(crate) const TABLINE_ARROW_WIDTH: u16 = 1;

/// The tabline's solved geometry for one frame: the two anchored block widths,
/// the windowed run of visible tabs, and the scroll arrows framing it.
///
/// [`draw_tabline`] paints from it and [`crate::hit_test`] maps a click to a tab
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
/// [`tabline_offset`](crate::snapshot::ClientSnapshot::tabline_offset) when it
/// is peeking, or — following the active tab — at the smallest index that keeps
/// the active tab on screen. A one-cell arrow is reserved on each side while
/// scrolled and drawn on whichever side still hides tabs.
pub(crate) fn tabline_layout(snapshot: &RenderSnapshot, area: RatatuiRect) -> TablineLayout {
    let right_width = right_block(snapshot).width() as u16;
    let right_x = area.right().saturating_sub(right_width).max(area.x);
    let session_width = (session_line(snapshot).width() as u16).min(right_x.saturating_sub(area.x));
    let strip_start = area.x.saturating_add(session_width).saturating_add(1);

    let count = snapshot.session.tabs_metadata.len();
    let widths: Vec<u16> = (0..count)
        .map(|i| tab_line(snapshot, i).width() as u16)
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

    let active = snapshot
        .session
        .tabs_metadata
        .iter()
        .position(|meta| meta.active)
        .unwrap_or(0);
    let first_visible = match snapshot.client.tabline_offset {
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

/// The tabline's right-anchored block: the mode tag. Each pane's scroll
/// position lives in its own bottom border (see [`draw_panes`]), not here.
fn right_block(snapshot: &RenderSnapshot) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {} ", mode_tags(&snapshot.client)),
        mode_style(&snapshot.theme),
    ))
}

/// The tabline's left-anchored block: the session name.
fn session_line(snapshot: &RenderSnapshot) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {} ", snapshot.session.name),
        session_style(&snapshot.theme),
    ))
}

/// One tab's two-block ribbon (`#N` block + name block) at metadata index
/// `meta_index`, styled on its own stop of the theme's chrome ramp.
fn tab_line(snapshot: &RenderSnapshot, meta_index: usize) -> Line<'static> {
    let count = snapshot.session.tabs_metadata.len();
    let meta = &snapshot.session.tabs_metadata[meta_index];
    Line::from(vec![
        Span::styled(
            format!(" #{} ", meta.index + 1),
            tab_index_style(&snapshot.theme, meta.active, meta_index, count),
        ),
        Span::styled(
            format!(" {} ", meta.name),
            tab_name_style(&snapshot.theme, meta.active, meta_index, count),
        ),
    ])
}
