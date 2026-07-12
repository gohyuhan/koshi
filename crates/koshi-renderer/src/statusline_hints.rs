//! Bottom keybinding bar with Zellij-style modifier groups and action ribbons.
//!
//! Idle view groups every top-level hint under one human modifier header such
//! as `Ctrl +` or `Alt +`; keys with the same action label fold into one ribbon.
//! A modifier-less key (bare `Tab`) is its own opener, so it wears the header
//! style itself rather than a block inside a neighboring group.
//! Pending view paints the pressed prefix as an accent breadcrumb, then shows
//! only its next chords. Internal config spellings such as `C-` and `A-` never
//! leak into user-facing text. Each modifier group takes one stop on the koshi
//! purple→blue ramp, matching the tab list above; hints that don't fit are
//! dropped whole with a trailing `…` marker.

use std::collections::BTreeMap;

use koshi_core::key::{Key, KeyChord, ModFlags, NamedKey};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Widget};

use crate::snapshot::{KeymapHints, RenderSnapshot};
use crate::theme;

const REVERT_MARKER: &str = " keys! ";

/// Paint one chrome-owned hint row.
pub fn draw_hint_bar(snapshot: &RenderSnapshot, area: RatatuiRect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    Clear.render(area, buf);

    let hints = &snapshot.keymap_hints;
    let pending = snapshot
        .client
        .pending_sequence
        .as_ref()
        .map_or(&[][..], |sequence| sequence.chords());
    let mut right_edge = area.right();
    if hints.reverted {
        let marker = Line::from(Span::styled(REVERT_MARKER, revert_style()));
        let width = marker.width() as u16;
        let x = right_edge.saturating_sub(width).max(area.x);
        set_line_clipped(buf, x, area.y, &marker, right_edge - x);
        right_edge = x;
    }

    let mut x = area.x;
    if !pending.is_empty() {
        for (index, chord) in pending.iter().enumerate() {
            let prefix_text = (index == 0).then(|| prefix_text(hints, *chord)).flatten();
            let line = chord_ribbon(*chord, prefix_text.as_deref());
            if !paint_whole(buf, &mut x, area.y, right_edge, &line) {
                draw_overflow_marker(buf, x, area.y, right_edge);
                return;
            }
        }
        let arrow = Line::from(Span::styled(" ▶ ", breadcrumb_arrow_style()));
        if !paint_whole(buf, &mut x, area.y, right_edge, &arrow) {
            draw_overflow_marker(buf, x, area.y, right_edge);
            return;
        }
    }

    let groups = display_groups(hint_items(hints, pending));
    let count = groups.len();
    for (group_index, group) in groups.into_iter().enumerate() {
        // A modifier-less binding has no header: its key IS the sequence's
        // first key, so it wears the header's plain-text style instead of a
        // continuation key's block — `Tab` reads as its own opener, not as
        // another key inside the preceding modifier group.
        let key_style = if group.mods.is_empty() {
            ramp_header_style(group_index, count)
        } else {
            ramp_key_style(group_index, count)
        };
        let label_style = ramp_label_style(group_index, count);
        let header = (!group.mods.is_empty()).then(|| {
            Line::from(Span::styled(
                format!(" {} + ", human_modifiers(group.mods)),
                ramp_header_style(group_index, count),
            ))
        });
        let first_width = group.entries.first().map_or(0, |entry| {
            entry_ribbon(entry, key_style, label_style).width() as u16
        });
        let header_width = header.as_ref().map_or(0, |line| line.width() as u16);
        if x.saturating_add(header_width).saturating_add(first_width) > right_edge {
            draw_overflow_marker(buf, x, area.y, right_edge);
            return;
        }
        if let Some(header) = header {
            let _ = paint_whole(buf, &mut x, area.y, right_edge, &header);
        }
        for entry in group.entries {
            let line = entry_ribbon(&entry, key_style, label_style);
            if !paint_whole(buf, &mut x, area.y, right_edge, &line) {
                draw_overflow_marker(buf, x, area.y, right_edge);
                return;
            }
        }
    }
}

/// Mark dropped trailing hints with `…` so truncation is visible. Painted at
/// the current cursor, or over the row's last cell when the hints consumed
/// the full width.
fn draw_overflow_marker(buf: &mut Buffer, x: u16, y: u16, right_edge: u16) {
    let x = x.min(right_edge.saturating_sub(1));
    let marker = Line::from(Span::styled("…", overflow_style()));
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
                (None, count) => {
                    let untouched = !bucket.any_user && !removed_under(hints, pending, chord);
                    let text = untouched
                        .then(|| hints.prefix_labels.get(&chord).cloned())
                        .flatten()
                        .unwrap_or_else(|| format!("+{count}"));
                    (text, false)
                }
            };
            HintItem {
                chord,
                text,
                pinned,
            }
        })
        .collect();
    items.sort_by_key(|item| {
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
        let group = match groups
            .iter_mut()
            .find(|group| group.mods == item.chord.mods)
        {
            Some(group) => group,
            None => {
                let index = groups.len();
                groups.push(DisplayGroup {
                    mods: item.chord.mods,
                    entries: Vec::new(),
                });
                &mut groups[index]
            }
        };
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
    let untouched = !any_user && !removed_under(hints, &[], chord);
    Some(
        untouched
            .then(|| hints.prefix_labels.get(&chord).cloned())
            .flatten()
            .unwrap_or_else(|| format!("+{count}")),
    )
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
fn chord_ribbon(chord: KeyChord, label: Option<&str>) -> Line<'static> {
    let mut spans = Vec::new();
    if !chord.mods.is_empty() {
        spans.push(Span::styled(
            format!(" {} + ", human_modifiers(chord.mods)),
            breadcrumb_modifier_style(),
        ));
    }
    spans.push(Span::styled(
        format!(" {} ", human_key(chord.key)),
        breadcrumb_key_style(),
    ));
    if let Some(label) = label {
        spans.push(Span::styled(format!(" {label} "), breadcrumb_key_style()));
    }
    Line::from(spans)
}

fn entry_ribbon(entry: &DisplayEntry, key_style: Style, label_style: Style) -> Line<'static> {
    let keys = entry
        .keys
        .iter()
        .map(|key| human_key(*key))
        .collect::<Vec<_>>()
        .join("");
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
    let width = line.width() as u16;
    if x.saturating_add(width) > right_edge {
        return false;
    }
    set_line_clipped(buf, *x, y, line, width);
    *x += width;
    true
}

fn set_line_clipped(buf: &mut Buffer, x: u16, y: u16, line: &Line<'_>, max_width: u16) {
    if y >= buf.area.top() && y < buf.area.bottom() {
        buf.set_line(x, y, line, max_width);
    }
}

/// A modifier group's `Ctrl +` header: its ramp stop as plain colored text.
fn ramp_header_style(index: usize, count: usize) -> Style {
    Style::default()
        .fg(theme::ramp(index, count))
        .add_modifier(Modifier::BOLD)
}

/// A group's key block: light text on the group's ramp stop.
fn ramp_key_style(index: usize, count: usize) -> Style {
    Style::default()
        .fg(theme::ON_RAMP)
        .bg(theme::ramp(index, count))
        .add_modifier(Modifier::BOLD)
}

/// A group's action-label block: the same stop dimmed, quiet text.
fn ramp_label_style(index: usize, count: usize) -> Style {
    Style::default()
        .fg(theme::ON_RAMP_DIM)
        .bg(theme::ramp_dim(index, count))
}

/// The pressed-prefix breadcrumb's modifier text: accent on the bar.
fn breadcrumb_modifier_style() -> Style {
    Style::default()
        .fg(theme::ACCENT)
        .add_modifier(Modifier::BOLD)
}

/// The pressed-prefix breadcrumb's key/label blocks: dark text on the
/// accent, brighter than any ramp stop, so the in-progress chords stand out.
fn breadcrumb_key_style() -> Style {
    Style::default()
        .fg(theme::ON_ACCENT)
        .bg(theme::ACCENT)
        .add_modifier(Modifier::BOLD)
}

fn breadcrumb_arrow_style() -> Style {
    Style::default()
        .fg(theme::ACCENT)
        .add_modifier(Modifier::BOLD)
}

/// The `…` marking hints dropped for width.
fn overflow_style() -> Style {
    Style::default()
        .fg(theme::ON_RAMP_DIM)
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
