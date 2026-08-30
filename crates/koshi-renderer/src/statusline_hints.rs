//! The statusline: the bottom keybinding row, with Zellij-style modifier
//! groups and action ribbons.
//!
//! Idle view groups every top-level hint under one human modifier header such
//! as `Ctrl +` or `Alt +`; keys with the same action label fold into one ribbon.
//! A modifier-less key (bare `Tab`) is its own opener and wears the header
//! style itself.
//! Pending view paints the pressed prefix as an accent breadcrumb, then shows
//! only its next chords. Internal config spellings such as `C-` and `A-` never
//! leak into user-facing text. The row is filled with the theme's bar
//! background (black by default) before anything is painted, and each modifier
//! group takes one stop on the theme's chrome ramp (light-purple → light-blue
//! by default), matching the tab list above; hints that don't fit are dropped
//! whole with a trailing `…` marker.

use std::collections::BTreeMap;

use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Widget};

use crate::region::StatuslineInputs;
use crate::render::{bar_style, line_width, set_line_clipped};
use crate::snapshot::KeymapHints;
use crate::theme::Theme;

const REVERT_MARKER: &str = " keys! ";

/// Paint the statusline from `inputs` — see [`StatuslineInputs`] — in
/// `theme`'s colors. `area` is the row to paint. `buf` is the buffer painted
/// into.
///
/// Does nothing for a zero-size area. Otherwise paints in this order:
///
/// 1. Blanks the row, then fills it with the theme's bar background.
/// 2. Draws the ` keys! ` marker against the right edge when the user keymap
///    was reverted. The marker holds that edge, and every hint below stops
///    short of it.
/// 3. Draws one accent ribbon per already-pressed chord of `pending`, left to
///    right, then a ` ▶ ` arrow. Only the first chord's ribbon carries that
///    chord's prefix label, and only when bindings sit under it.
/// 4. Draws each modifier group left to right: its ` Ctrl + ` header, then one
///    two-block ribbon per action.
/// 5. Draws a `…` marker where the row ran out of room, and stops there.
pub(crate) fn draw_statusline(
    inputs: StatuslineInputs<'_>,
    theme: &Theme,
    area: RatatuiRect,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let StatuslineInputs { hints, pending } = inputs;
    // Clear drops stale cells, then the bar background fills the row whole.
    // Ribbons painted after this set their own background; plain text such as
    // a `Ctrl +` header sets only a foreground and keeps this fill.
    Clear.render(area, buf);
    buf.set_style(area, bar_style(theme));

    let pending = pending.map_or(&[][..], KeySequence::chords);
    let mut right_edge = area.right();
    if hints.reverted {
        let marker = Line::from(Span::styled(REVERT_MARKER, revert_style()));
        let width = line_width(&marker);
        let x = right_edge.saturating_sub(width).max(area.x);
        set_line_clipped(buf, x, area.y, &marker, right_edge - x);
        right_edge = x;
    }

    let mut x = area.x;
    if !pending.is_empty() {
        for (index, chord) in pending.iter().enumerate() {
            let label = if index == 0 {
                prefix_text(hints, *chord)
            } else {
                None
            };
            let line = chord_ribbon(theme, *chord, label.as_deref());
            if !paint_whole(buf, &mut x, area.y, right_edge, &line) {
                draw_overflow_marker(buf, theme, x, area.y, area.x, right_edge);
                return;
            }
        }
        let arrow = Line::from(Span::styled(" ▶ ", breadcrumb_arrow_style(theme)));
        if !paint_whole(buf, &mut x, area.y, right_edge, &arrow) {
            draw_overflow_marker(buf, theme, x, area.y, area.x, right_edge);
            return;
        }
    }

    let groups = display_groups(hint_items(hints, pending));
    let count = groups.len();
    for (group_index, group) in groups.into_iter().enumerate() {
        // A modifier-less group has no header: its key takes the header's
        // plain-text style instead of a key block.
        let key_style = if group.mods.is_empty() {
            ramp_header_style(theme, group_index, count)
        } else {
            ramp_key_style(theme, group_index, count)
        };
        let label_style = ramp_label_style(theme, group_index, count);
        let header = (!group.mods.is_empty()).then(|| {
            Line::from(Span::styled(
                format!(" {} + ", human_modifiers(group.mods)),
                ramp_header_style(theme, group_index, count),
            ))
        });
        let first_width = group.entries.first().map_or(0, |entry| {
            line_width(&entry_ribbon(entry, key_style, label_style))
        });
        let header_width = header.as_ref().map_or(0, line_width);
        if x.saturating_add(header_width).saturating_add(first_width) > right_edge {
            draw_overflow_marker(buf, theme, x, area.y, area.x, right_edge);
            return;
        }
        if let Some(header) = header {
            let _ = paint_whole(buf, &mut x, area.y, right_edge, &header);
        }
        for entry in group.entries {
            let line = entry_ribbon(&entry, key_style, label_style);
            if !paint_whole(buf, &mut x, area.y, right_edge, &line) {
                draw_overflow_marker(buf, theme, x, area.y, area.x, right_edge);
                return;
            }
        }
    }
}

/// Mark dropped trailing hints with `…`. Painted at the current cursor, or
/// over the row's last cell when the hints consumed the full width.
///
/// The column is held inside `left_edge..right_edge`: a `right_edge` the
/// revert marker pulled down to `left_edge` paints on `left_edge`, never one
/// column left of it.
fn draw_overflow_marker(
    buf: &mut Buffer,
    theme: &Theme,
    x: u16,
    y: u16,
    left_edge: u16,
    right_edge: u16,
) {
    let x = x.min(right_edge.saturating_sub(1)).max(left_edge);
    let marker = Line::from(Span::styled("…", overflow_style(theme)));
    set_line_clipped(buf, x, y, &marker, 1);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HintItem {
    chord: KeyChord,
    text: String,
    pinned: bool,
}

#[derive(Default)]
struct ChordBucket {
    leaf: Option<(String, bool)>,
    deeper: usize,
    any_user: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct DisplayGroup {
    mods: ModFlags,
    entries: Vec<DisplayEntry>,
}

#[derive(Debug, PartialEq, Eq)]
struct DisplayEntry {
    keys: Vec<Key>,
    text: String,
    pinned: bool,
}

fn hint_items(hints: &KeymapHints, pending: &[KeyChord]) -> Vec<HintItem> {
    let mut buckets: BTreeMap<KeyChord, ChordBucket> = BTreeMap::new();
    for entry in hints.entries.iter() {
        let chords = entry.sequence.chords();
        if chords.len() <= pending.len() || &chords[..pending.len()] != pending {
            continue;
        }
        let chord = chords[pending.len()];
        let bucket = buckets.entry(chord).or_default();
        if chords.len() == pending.len() + 1 {
            bucket.leaf = Some((entry.label.clone(), entry.pinned));
        } else {
            bucket.deeper += 1;
        }
        bucket.any_user |= entry.user_set;
    }

    let mut items: Vec<_> = buckets
        .into_iter()
        .map(|(chord, bucket)| {
            let (text, pinned) = match (bucket.leaf, bucket.deeper) {
                (Some((label, pinned)), 0) => (label, pinned),
                (Some((label, pinned)), count) => (format!("{label} +{count}"), pinned),
                (None, count) => (
                    prefix_label(hints, pending, chord, count, bucket.any_user),
                    false,
                ),
            };
            HintItem {
                chord,
                text,
                pinned,
            }
        })
        .collect();
    // Pinned hints first, then by modifier group, then by key.
    items.sort_by_cached_key(|item| {
        (
            !item.pinned,
            modifier_rank(item.chord.mods),
            key_rank(item.chord.key),
        )
    });
    items
}

fn display_groups(items: Vec<HintItem>) -> Vec<DisplayGroup> {
    let mut groups: Vec<DisplayGroup> = Vec::new();
    for item in items {
        let mods = item.chord.mods;
        let index = match groups.iter().position(|group| group.mods == mods) {
            Some(index) => index,
            None => {
                groups.push(DisplayGroup {
                    mods,
                    entries: Vec::new(),
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[index];
        if let Some(entry) = group
            .entries
            .iter_mut()
            .find(|entry| entry.text == item.text && entry.pinned == item.pinned)
        {
            entry.keys.push(item.chord.key);
        } else {
            group.entries.push(DisplayEntry {
                keys: vec![item.chord.key],
                text: item.text,
                pinned: item.pinned,
            });
        }
    }
    groups.sort_by_key(|group| modifier_rank(group.mods));
    groups
}

fn prefix_text(hints: &KeymapHints, chord: KeyChord) -> Option<String> {
    let mut count = 0;
    let mut any_user = false;
    for entry in hints.entries.iter() {
        let chords = entry.sequence.chords();
        if chords.len() > 1 && chords[0] == chord {
            count += 1;
            any_user |= entry.user_set;
        }
    }
    if count == 0 {
        return None;
    }
    Some(prefix_label(hints, &[], chord, count, any_user))
}

/// The text a prefix chord shows: its shipped label, or a `+N` marker counting
/// the `count` bindings under it.
///
/// The shipped label comes from `hints.prefix_labels`. The `+N` marker stands
/// when that map has no entry for `chord`, when `any_user` says a user surface
/// authored a binding under the prefix, or when [`removed_under`] says one was
/// removed there.
fn prefix_label(
    hints: &KeymapHints,
    pending: &[KeyChord],
    chord: KeyChord,
    count: usize,
    any_user: bool,
) -> String {
    if !any_user && !removed_under(hints, pending, chord) {
        if let Some(label) = hints.prefix_labels.get(&chord) {
            return label.clone();
        }
    }
    format!("+{count}")
}

fn removed_under(hints: &KeymapHints, pending: &[KeyChord], chord: KeyChord) -> bool {
    hints.removed.iter().any(|sequence| {
        let chords = sequence.chords();
        chords.len() > pending.len()
            && &chords[..pending.len()] == pending
            && chords[pending.len()] == chord
    })
}

/// The accent ribbon for one already-pressed chord of the pending sequence.
fn chord_ribbon(theme: &Theme, chord: KeyChord, label: Option<&str>) -> Line<'static> {
    let mut spans = Vec::new();
    if !chord.mods.is_empty() {
        spans.push(Span::styled(
            format!(" {} + ", human_modifiers(chord.mods)),
            breadcrumb_modifier_style(theme),
        ));
    }
    spans.push(Span::styled(
        format!(" {} ", human_key(chord.key)),
        breadcrumb_key_style(theme),
    ));
    if let Some(label) = label {
        spans.push(Span::styled(
            format!(" {label} "),
            breadcrumb_key_style(theme),
        ));
    }
    Line::from(spans)
}

fn entry_ribbon(entry: &DisplayEntry, key_style: Style, label_style: Style) -> Line<'static> {
    let keys: String = entry.keys.iter().map(|key| human_key(*key)).collect();
    Line::from(vec![
        Span::styled(format!(" {keys} "), key_style),
        Span::styled(format!(" {} ", entry.text), label_style),
    ])
}

fn human_modifiers(mods: ModFlags) -> String {
    let mut names = Vec::new();
    if mods.contains(ModFlags::CTRL) {
        names.push("Ctrl");
    }
    if mods.contains(ModFlags::ALT) {
        names.push("Alt");
    }
    if mods.contains(ModFlags::SHIFT) {
        names.push("Shift");
    }
    if mods.contains(ModFlags::SUPER) {
        names.push("Super");
    }
    names.join("+")
}

fn human_key(key: Key) -> String {
    match key {
        Key::Char(c) => c.to_string(),
        Key::Named(NamedKey::Left) => "←".to_owned(),
        Key::Named(NamedKey::Down) => "↓".to_owned(),
        Key::Named(NamedKey::Up) => "↑".to_owned(),
        Key::Named(NamedKey::Right) => "→".to_owned(),
        Key::Named(NamedKey::Enter) => "ENTER".to_owned(),
        Key::Named(NamedKey::Backspace) => "BACKSPACE".to_owned(),
        Key::Named(NamedKey::Esc) => "ESC".to_owned(),
        Key::Named(NamedKey::Space) => "SPACE".to_owned(),
        Key::Named(named) => named.to_string(),
    }
}

fn modifier_rank(mods: ModFlags) -> u16 {
    match mods.bits() {
        1 => 0, // Ctrl
        2 => 1, // Alt
        5 => 2, // Ctrl+Shift
        4 => 3, // Shift
        8 => 4, // Super
        bits => 5 + u16::from(bits),
    }
}

fn key_rank(key: Key) -> (u8, String) {
    let direction = match key {
        Key::Named(NamedKey::Left) => 0,
        Key::Named(NamedKey::Down) => 1,
        Key::Named(NamedKey::Up) => 2,
        Key::Named(NamedKey::Right) => 3,
        _ => 4,
    };
    (direction, human_key(key))
}

fn paint_whole(buf: &mut Buffer, x: &mut u16, y: u16, right_edge: u16, line: &Line<'_>) -> bool {
    let width = line_width(line);
    if x.saturating_add(width) > right_edge {
        return false;
    }
    set_line_clipped(buf, *x, y, line, width);
    *x += width;
    true
}

/// A modifier group's `Ctrl +` header: its ramp stop as plain colored text.
fn ramp_header_style(theme: &Theme, index: usize, count: usize) -> Style {
    Style::default()
        .fg(theme.ramp(index, count))
        .add_modifier(Modifier::BOLD)
}

/// A group's key block: light text on the group's ramp stop.
fn ramp_key_style(theme: &Theme, index: usize, count: usize) -> Style {
    Style::default()
        .fg(theme.on_ramp)
        .bg(theme.ramp(index, count))
        .add_modifier(Modifier::BOLD)
}

/// A group's action-label block: the same stop dimmed, quiet text.
fn ramp_label_style(theme: &Theme, index: usize, count: usize) -> Style {
    Style::default()
        .fg(theme.on_ramp_dim)
        .bg(theme.ramp_dim(index, count))
}

/// The pressed-prefix breadcrumb's modifier text: accent on the bar.
fn breadcrumb_modifier_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD)
}

/// The pressed-prefix breadcrumb's key/label blocks: dark text on the accent.
fn breadcrumb_key_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.on_accent)
        .bg(theme.accent)
        .add_modifier(Modifier::BOLD)
}

fn breadcrumb_arrow_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD)
}

/// The `…` marking hints dropped for width.
fn overflow_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.on_ramp_dim)
        .add_modifier(Modifier::BOLD)
}

fn revert_style() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(Color::Red)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests;
