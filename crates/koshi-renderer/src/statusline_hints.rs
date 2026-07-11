//! The keybinding hint bar: the bottom statusline row showing what keys do in
//! the client's current input mode.
//!
//! The bar has two faces, switched by the client's pending key sequence:
//!
//! - **Idle** (no sequence pending): the mode's top-level view. A single-chord
//!   binding shows as `<key> Action` (`<C-l> Lock`); the multi-chord bindings
//!   sharing an opening chord collapse into one group hint — labeled
//!   (`<C-p> PANE`) while every binding under the chord is an untouched
//!   default with a shipped label, or a derived `<C-p> +2` marker once any
//!   user surface overrides, adds, or removes a binding under it.
//! - **Pending** (a multi-chord sequence underway): a breadcrumb of the
//!   chords pressed so far, then the continuations one more chord reaches —
//!   `<C-p> PANE ▸ n New Pane  x Close Pane`. Deeper groups nest the same
//!   way.
//!
//! Hints render in key order, pinned entries first (the locked-mode unlock
//! binding), and truncate by dropping whole trailing hints — the bar never
//! wraps. When the user keymap was reverted to defaults over a key collision,
//! a red `[keys!]` marker holds the row's right edge.
//!
//! All data arrives resolved in [`KeymapHints`] — bindings joined to display
//! names, labels, removals, and the revert flag — so this module is pure
//! presentation over the snapshot.

use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Widget};

use koshi_core::key::KeyChord;

use crate::snapshot::{KeymapHints, RenderSnapshot};

/// Paint the hint bar for `snapshot`'s client into the one-row `area`.
///
/// Blanks the row first — it is koshi-owned chrome, so no pane cell shows
/// through even in a mode with nothing to hint — then draws the idle or
/// pending face from the client's `pending_sequence`, with the revert marker
/// right-aligned when the keymap was reverted. Does nothing for a zero-size
/// area.
pub fn draw_hint_bar(snapshot: &RenderSnapshot, area: RatatuiRect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    Clear.render(area, buf);

    let hints = &snapshot.keymap_hints;
    let pending: &[KeyChord] = snapshot
        .client
        .pending_sequence
        .as_ref()
        .map_or(&[], |sequence| sequence.chords());

    // The revert marker owns the right edge; hints truncate short of it.
    let mut right_edge = area.right();
    if hints.reverted {
        let marker = Line::from(Span::styled(REVERT_MARKER, hint_revert_style()));
        let width = marker.width() as u16;
        let x = right_edge.saturating_sub(width).max(area.x);
        set_line_clipped(buf, x, area.y, &marker, right_edge - x);
        right_edge = x.saturating_sub(1).max(area.x);
    }

    let mut x = area.x;

    // Breadcrumb: the chords pressed so far, labeled when the opening chord
    // carries a shipped label, then the continuation arrow.
    if !pending.is_empty() {
        let mut spans = Vec::new();
        for (i, chord) in pending.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(chord.to_string(), hint_breadcrumb_style()));
            if let Some(label) = hints.prefix_labels.get(chord) {
                spans.push(Span::styled(format!(" {label}"), hint_breadcrumb_style()));
            }
        }
        spans.push(Span::styled(" ▸ ", hint_label_style()));
        let breadcrumb = Line::from(spans);
        let width = (breadcrumb.width() as u16).min(right_edge.saturating_sub(x));
        set_line_clipped(buf, x, area.y, &breadcrumb, width);
        x += width;
    }

    // Hints in key order, pinned first; a hint that no longer fits whole is
    // dropped along with everything after it.
    for item in hint_items(hints, pending) {
        let line = Line::from(vec![
            Span::styled(item.key, hint_key_style()),
            Span::raw(" "),
            Span::styled(item.text, hint_label_style()),
        ]);
        let width = line.width() as u16;
        if x + width > right_edge {
            break;
        }
        set_line_clipped(buf, x, area.y, &line, width);
        x += width + 2;
    }
}

/// The right-aligned marker shown while the user keymap is reverted to
/// defaults over a key collision.
const REVERT_MARKER: &str = "[keys!]";

/// One rendered hint: the key part, the text after it, and whether
/// truncation must keep it.
struct HintItem {
    /// The chord's canonical text (`<C-p>`, `n`).
    key: String,
    /// The action name, the group label, or the `+N` marker.
    text: String,
    /// True for the hint truncation never drops (the locked-mode unlock).
    pinned: bool,
}

/// What one continuation chord leads to: a directly bound action, deeper
/// sequences, or both.
#[derive(Default)]
struct ChordBucket {
    /// The action label of the binding this chord completes, if one does,
    /// and whether that binding is pinned.
    leaf: Option<(String, bool)>,
    /// How many bindings continue past this chord.
    deeper: usize,
    /// Whether any binding under this chord is user-authored, which voids
    /// the group's shipped label.
    any_user: bool,
}

/// The hints to show for `pending`: every binding one more chord advances,
/// folded per continuation chord, pinned entries first.
///
/// A chord that completes a binding shows that action's name; a chord more
/// bindings continue past shows the group — its shipped label while the
/// group is untouched defaults, a `+N` count otherwise. A chord that does
/// both shows `Action +N`. A user removal under a labeled chord voids the
/// label the same way a user entry does: the shipped name no longer
/// describes the set.
fn hint_items(hints: &KeymapHints, pending: &[KeyChord]) -> Vec<HintItem> {
    let mut buckets: BTreeMap<KeyChord, ChordBucket> = BTreeMap::new();

    for entry in hints.entries.iter() {
        let chords = entry.sequence.chords();
        if chords.len() <= pending.len() || &chords[..pending.len()] != pending {
            continue;
        }
        let bucket = buckets.entry(chords[pending.len()]).or_default();
        if chords.len() == pending.len() + 1 {
            bucket.leaf = Some((entry.label.clone(), entry.pinned));
        } else {
            bucket.deeper += 1;
        }
        bucket.any_user |= entry.user_set;
    }

    let mut items: Vec<HintItem> = buckets
        .into_iter()
        .map(|(chord, bucket)| {
            let (text, pinned) = match (bucket.leaf, bucket.deeper) {
                (Some((label, pinned)), 0) => (label, pinned),
                (Some((label, pinned)), n) => (format!("{label} +{n}"), pinned),
                (None, n) => {
                    let pure = !bucket.any_user && !removed_under(hints, pending, chord);
                    let text = pure
                        .then(|| hints.prefix_labels.get(&chord).cloned())
                        .flatten()
                        .unwrap_or_else(|| format!("+{n}"));
                    (text, false)
                }
            };
            HintItem {
                key: chord.to_string(),
                text,
                pinned,
            }
        })
        .collect();
    items.sort_by_key(|item| !item.pinned);
    items
}

/// Whether any user-removed key in the current mode sits under
/// `pending + chord` — including that exact sequence.
fn removed_under(hints: &KeymapHints, pending: &[KeyChord], chord: KeyChord) -> bool {
    hints.removed.iter().any(|sequence| {
        let chords = sequence.chords();
        chords.len() > pending.len()
            && &chords[..pending.len()] == pending
            && chords[pending.len()] == chord
    })
}

/// Write `line` at `(x, y)` clipped to `max_width`, skipping rows outside the
/// buffer — the same resize guard the frame's other chrome rows use.
fn set_line_clipped(buf: &mut Buffer, x: u16, y: u16, line: &Line<'_>, max_width: u16) {
    if y < buf.area.top() || y >= buf.area.bottom() {
        return;
    }
    buf.set_line(x, y, line, max_width);
}

/// Accent style on a hint's key glyph.
fn hint_key_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Dim style on a hint's action name, group label, or `+N` marker.
fn hint_label_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Inverted accent on the pending-sequence breadcrumb, marking the chords
/// already pressed.
fn hint_breadcrumb_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

/// Alarm style on the keymap-revert marker.
fn hint_revert_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests;
