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
use crate::render::{compute_bar_style, get_line_width, set_line_clipped};
use crate::snapshot::KeymapHints;
use crate::theme::Theme;

const REVERT_MARKER: &str = " keys! ";

/// Paint the statusline from [`StatuslineInputs`] in `theme`'s colors.
/// `statusline_area` is the row to paint into `buffer`.
///
/// Does nothing for a zero-size area. Otherwise paints in this order:
///
/// 1. Blanks the row, then fills it with the theme's bar background.
/// 2. Draws the ` keys! ` marker against the right edge when the user keymap
///    was reverted. The marker holds that edge, and every hint below stops
///    short of it.
/// 3. Draws one accent ribbon per already-pressed chord of `pending_key_sequence`, left to
///    right, then a ` ▶ ` arrow. Only the first chord's ribbon carries that
///    chord's prefix label, and only when bindings sit under it.
/// 4. Draws each modifier group left to right: its ` Ctrl + ` header, then one
///    two-block ribbon per action.
/// 5. Draws a `…` marker where the row ran out of room, and stops there.
pub(crate) fn draw_statusline(
    statusline_inputs: StatuslineInputs<'_>,
    theme: &Theme,
    statusline_area: RatatuiRect,
    buffer: &mut Buffer,
) {
    if statusline_area.width == 0 || statusline_area.height == 0 {
        return;
    }
    let StatuslineInputs {
        keymap_hints,
        pending_key_sequence,
    } = statusline_inputs;
    // Clear drops stale cells, then the bar background fills the row whole.
    // Ribbons painted after this set their own background; plain text such as
    // a `Ctrl +` header sets only a foreground and keeps this fill.
    Clear.render(statusline_area, buffer);
    buffer.set_style(statusline_area, compute_bar_style(theme));

    let pending_chords = pending_key_sequence.map_or(&[][..], KeySequence::list_chords);
    let mut right_edge_column = statusline_area.right();
    if keymap_hints.is_reverted_to_defaults {
        let revert_marker_line =
            Line::from(Span::styled(REVERT_MARKER, compute_revert_marker_style()));
        let revert_marker_width = get_line_width(&revert_marker_line);
        let revert_marker_start_column = right_edge_column
            .saturating_sub(revert_marker_width)
            .max(statusline_area.x);
        set_line_clipped(
            buffer,
            revert_marker_start_column,
            statusline_area.y,
            &revert_marker_line,
            right_edge_column - revert_marker_start_column,
        );
        right_edge_column = revert_marker_start_column;
    }

    let mut current_column = statusline_area.x;
    if !pending_chords.is_empty() {
        for (pending_chord_index, pending_chord) in pending_chords.iter().enumerate() {
            let prefix_label = if pending_chord_index == 0 {
                find_prefix_label(keymap_hints, *pending_chord)
            } else {
                None
            };
            let chord_line = build_chord_ribbon(theme, *pending_chord, prefix_label.as_deref());
            if !paint_line_if_fits(
                buffer,
                &mut current_column,
                statusline_area.y,
                right_edge_column,
                &chord_line,
            ) {
                draw_overflow_marker(
                    buffer,
                    theme,
                    current_column,
                    statusline_area.y,
                    statusline_area.x,
                    right_edge_column,
                );
                return;
            }
        }
        let breadcrumb_arrow_line =
            Line::from(Span::styled(" ▶ ", compute_breadcrumb_arrow_style(theme)));
        if !paint_line_if_fits(
            buffer,
            &mut current_column,
            statusline_area.y,
            right_edge_column,
            &breadcrumb_arrow_line,
        ) {
            draw_overflow_marker(
                buffer,
                theme,
                current_column,
                statusline_area.y,
                statusline_area.x,
                right_edge_column,
            );
            return;
        }
    }

    let modifier_groups = group_hint_entries(list_hint_entries(keymap_hints, pending_chords));
    let modifier_group_count = modifier_groups.len();
    for (modifier_group_index, modifier_group) in modifier_groups.into_iter().enumerate() {
        // A modifier-less group has no header: its key takes the header's
        // plain-text style instead of a key block.
        let key_style = if modifier_group.modifier_flags.is_empty() {
            compute_ramp_header_style(theme, modifier_group_index, modifier_group_count)
        } else {
            compute_ramp_key_style(theme, modifier_group_index, modifier_group_count)
        };
        let label_style =
            compute_ramp_label_style(theme, modifier_group_index, modifier_group_count);
        let header_line = (!modifier_group.modifier_flags.is_empty()).then(|| {
            Line::from(Span::styled(
                format!(
                    " {} + ",
                    format_modifier_names(modifier_group.modifier_flags)
                ),
                compute_ramp_header_style(theme, modifier_group_index, modifier_group_count),
            ))
        });
        let first_ribbon_width = modifier_group
            .action_ribbons
            .first()
            .map_or(0, |action_ribbon| {
                get_line_width(&build_action_ribbon(action_ribbon, key_style, label_style))
            });
        let header_width = header_line.as_ref().map_or(0, get_line_width);
        if current_column
            .saturating_add(header_width)
            .saturating_add(first_ribbon_width)
            > right_edge_column
        {
            draw_overflow_marker(
                buffer,
                theme,
                current_column,
                statusline_area.y,
                statusline_area.x,
                right_edge_column,
            );
            return;
        }
        if let Some(header_line) = header_line {
            let _ = paint_line_if_fits(
                buffer,
                &mut current_column,
                statusline_area.y,
                right_edge_column,
                &header_line,
            );
        }
        for action_ribbon in modifier_group.action_ribbons {
            let action_line = build_action_ribbon(&action_ribbon, key_style, label_style);
            if !paint_line_if_fits(
                buffer,
                &mut current_column,
                statusline_area.y,
                right_edge_column,
                &action_line,
            ) {
                draw_overflow_marker(
                    buffer,
                    theme,
                    current_column,
                    statusline_area.y,
                    statusline_area.x,
                    right_edge_column,
                );
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
    buffer: &mut Buffer,
    theme: &Theme,
    current_column: u16,
    row_index: u16,
    left_edge_column: u16,
    right_edge_column: u16,
) {
    let marker_start_column = current_column
        .min(right_edge_column.saturating_sub(1))
        .max(left_edge_column);
    let overflow_marker_line = Line::from(Span::styled("…", compute_overflow_style(theme)));
    set_line_clipped(
        buffer,
        marker_start_column,
        row_index,
        &overflow_marker_line,
        1,
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HintEntry {
    chord: KeyChord,
    action_label: String,
    is_pinned: bool,
}

#[derive(Default)]
struct ChordSummary {
    leaf_action: Option<LeafAction>,
    deeper_binding_count: usize,
    has_user_binding: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct ModifierGroup {
    modifier_flags: ModFlags,
    action_ribbons: Vec<ActionRibbon>,
}

#[derive(Debug, PartialEq, Eq)]
struct ActionRibbon {
    keys: Vec<Key>,
    action_label: String,
    is_pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeafAction {
    action_label: String,
    is_pinned: bool,
}

fn list_hint_entries(keymap_hints: &KeymapHints, pending_chords: &[KeyChord]) -> Vec<HintEntry> {
    let mut summary_by_chord: BTreeMap<KeyChord, ChordSummary> = BTreeMap::new();
    for hint_binding in keymap_hints.hint_bindings.iter() {
        let binding_chords = hint_binding.key_sequence.list_chords();
        if binding_chords.len() <= pending_chords.len()
            || &binding_chords[..pending_chords.len()] != pending_chords
        {
            continue;
        }
        let next_chord = binding_chords[pending_chords.len()];
        let chord_summary = summary_by_chord.entry(next_chord).or_default();
        if binding_chords.len() == pending_chords.len() + 1 {
            chord_summary.leaf_action = Some(LeafAction {
                action_label: hint_binding.action_display_name.clone(),
                is_pinned: hint_binding.is_pinned,
            });
        } else {
            chord_summary.deeper_binding_count += 1;
        }
        chord_summary.has_user_binding |= hint_binding.is_user_authored;
    }

    let mut hint_entries: Vec<HintEntry> = summary_by_chord
        .into_iter()
        .map(|(chord, chord_summary)| {
            let (action_label, is_pinned) = match (
                chord_summary.leaf_action,
                chord_summary.deeper_binding_count,
            ) {
                (Some(leaf_action), 0) => (leaf_action.action_label, leaf_action.is_pinned),
                (Some(leaf_action), deeper_binding_count) => (
                    format!("{} +{deeper_binding_count}", leaf_action.action_label),
                    leaf_action.is_pinned,
                ),
                (None, deeper_binding_count) => (
                    format_prefix_label(
                        keymap_hints,
                        pending_chords,
                        chord,
                        deeper_binding_count,
                        chord_summary.has_user_binding,
                    ),
                    false,
                ),
            };
            HintEntry {
                chord,
                action_label,
                is_pinned,
            }
        })
        .collect();
    // Pinned hints first, then by modifier group, then by key.
    hint_entries.sort_by_cached_key(|hint_entry| {
        (
            !hint_entry.is_pinned,
            compute_modifier_sort_rank(hint_entry.chord.modifier_flags),
            compute_key_sort_rank(hint_entry.chord.key),
        )
    });
    hint_entries
}

fn group_hint_entries(hint_entries: Vec<HintEntry>) -> Vec<ModifierGroup> {
    let mut modifier_groups: Vec<ModifierGroup> = Vec::new();
    for hint_entry in hint_entries {
        let modifier_flags = hint_entry.chord.modifier_flags;
        let modifier_group_index = match modifier_groups
            .iter()
            .position(|modifier_group| modifier_group.modifier_flags == modifier_flags)
        {
            Some(modifier_group_index) => modifier_group_index,
            None => {
                modifier_groups.push(ModifierGroup {
                    modifier_flags,
                    action_ribbons: Vec::new(),
                });
                modifier_groups.len() - 1
            }
        };
        let modifier_group = &mut modifier_groups[modifier_group_index];
        if let Some(action_ribbon) =
            modifier_group
                .action_ribbons
                .iter_mut()
                .find(|action_ribbon| {
                    action_ribbon.action_label == hint_entry.action_label
                        && action_ribbon.is_pinned == hint_entry.is_pinned
                })
        {
            action_ribbon.keys.push(hint_entry.chord.key);
        } else {
            modifier_group.action_ribbons.push(ActionRibbon {
                keys: vec![hint_entry.chord.key],
                action_label: hint_entry.action_label,
                is_pinned: hint_entry.is_pinned,
            });
        }
    }
    modifier_groups
        .sort_by_key(|modifier_group| compute_modifier_sort_rank(modifier_group.modifier_flags));
    modifier_groups
}

fn find_prefix_label(keymap_hints: &KeymapHints, opening_chord: KeyChord) -> Option<String> {
    let mut deeper_binding_count = 0;
    let mut has_user_binding = false;
    for hint_binding in keymap_hints.hint_bindings.iter() {
        let binding_chords = hint_binding.key_sequence.list_chords();
        if binding_chords.len() > 1 && binding_chords[0] == opening_chord {
            deeper_binding_count += 1;
            has_user_binding |= hint_binding.is_user_authored;
        }
    }
    if deeper_binding_count == 0 {
        return None;
    }
    Some(format_prefix_label(
        keymap_hints,
        &[],
        opening_chord,
        deeper_binding_count,
        has_user_binding,
    ))
}

/// The text a prefix chord shows: its shipped label, or a `+N` marker counting
/// the `binding_count` bindings under it.
///
/// The shipped label comes from `keymap_hints.prefix_labels`. The `+N` marker
/// stands when that map has no entry for `opening_chord`, when
/// `has_user_binding` says a user surface authored a binding under the prefix,
/// or when [`has_removed_binding_under_prefix`] says one was removed there.
fn format_prefix_label(
    keymap_hints: &KeymapHints,
    pending_chords: &[KeyChord],
    opening_chord: KeyChord,
    binding_count: usize,
    has_user_binding: bool,
) -> String {
    if !has_user_binding
        && !has_removed_binding_under_prefix(keymap_hints, pending_chords, opening_chord)
    {
        if let Some(prefix_label) = keymap_hints.prefix_labels.get(&opening_chord) {
            return prefix_label.clone();
        }
    }
    format!("+{binding_count}")
}

fn has_removed_binding_under_prefix(
    keymap_hints: &KeymapHints,
    pending_chords: &[KeyChord],
    opening_chord: KeyChord,
) -> bool {
    keymap_hints
        .removed_key_sequences
        .iter()
        .any(|removed_sequence| {
            let removed_chords = removed_sequence.list_chords();
            removed_chords.len() > pending_chords.len()
                && &removed_chords[..pending_chords.len()] == pending_chords
                && removed_chords[pending_chords.len()] == opening_chord
        })
}

/// The accent ribbon for one already-pressed chord of the pending sequence.
fn build_chord_ribbon(theme: &Theme, chord: KeyChord, prefix_label: Option<&str>) -> Line<'static> {
    let mut spans = Vec::new();
    if !chord.modifier_flags.is_empty() {
        spans.push(Span::styled(
            format!(" {} + ", format_modifier_names(chord.modifier_flags)),
            compute_breadcrumb_modifier_style(theme),
        ));
    }
    spans.push(Span::styled(
        format!(" {} ", format_key_name(chord.key)),
        compute_breadcrumb_key_style(theme),
    ));
    if let Some(prefix_label) = prefix_label {
        spans.push(Span::styled(
            format!(" {prefix_label} "),
            compute_breadcrumb_key_style(theme),
        ));
    }
    Line::from(spans)
}

fn build_action_ribbon(
    action_ribbon: &ActionRibbon,
    key_style: Style,
    label_style: Style,
) -> Line<'static> {
    let key_text: String = action_ribbon
        .keys
        .iter()
        .map(|key| format_key_name(*key))
        .collect();
    Line::from(vec![
        Span::styled(format!(" {key_text} "), key_style),
        Span::styled(format!(" {} ", action_ribbon.action_label), label_style),
    ])
}

fn format_modifier_names(modifier_flags: ModFlags) -> String {
    let mut modifier_names = Vec::new();
    if modifier_flags.has_all_modifiers(ModFlags::CTRL) {
        modifier_names.push("Ctrl");
    }
    if modifier_flags.has_all_modifiers(ModFlags::ALT) {
        modifier_names.push("Alt");
    }
    if modifier_flags.has_all_modifiers(ModFlags::SHIFT) {
        modifier_names.push("Shift");
    }
    if modifier_flags.has_all_modifiers(ModFlags::SUPER) {
        modifier_names.push("Super");
    }
    modifier_names.join("+")
}

fn format_key_name(key: Key) -> String {
    match key {
        Key::Char(character) => character.to_string(),
        Key::Named(NamedKey::Left) => "←".to_owned(),
        Key::Named(NamedKey::Down) => "↓".to_owned(),
        Key::Named(NamedKey::Up) => "↑".to_owned(),
        Key::Named(NamedKey::Right) => "→".to_owned(),
        Key::Named(NamedKey::Enter) => "ENTER".to_owned(),
        Key::Named(NamedKey::Backspace) => "BACKSPACE".to_owned(),
        Key::Named(NamedKey::Esc) => "ESC".to_owned(),
        Key::Named(NamedKey::Space) => "SPACE".to_owned(),
        Key::Named(named_key) => named_key.to_string(),
    }
}

fn compute_modifier_sort_rank(modifier_flags: ModFlags) -> u16 {
    match modifier_flags.bits() {
        1 => 0, // Ctrl
        2 => 1, // Alt
        5 => 2, // Ctrl+Shift
        4 => 3, // Shift
        8 => 4, // Super
        modifier_bits => 5 + u16::from(modifier_bits),
    }
}

fn compute_key_sort_rank(key: Key) -> (u8, String) {
    let arrow_order = match key {
        Key::Named(NamedKey::Left) => 0,
        Key::Named(NamedKey::Down) => 1,
        Key::Named(NamedKey::Up) => 2,
        Key::Named(NamedKey::Right) => 3,
        _ => 4,
    };
    (arrow_order, format_key_name(key))
}

fn paint_line_if_fits(
    buffer: &mut Buffer,
    current_column: &mut u16,
    row_index: u16,
    right_edge_column: u16,
    line: &Line<'_>,
) -> bool {
    let line_width = get_line_width(line);
    if current_column.saturating_add(line_width) > right_edge_column {
        return false;
    }
    set_line_clipped(buffer, *current_column, row_index, line, line_width);
    *current_column += line_width;
    true
}

/// A modifier group's `Ctrl +` header: its ramp stop as plain colored text.
fn compute_ramp_header_style(
    theme: &Theme,
    modifier_group_index: usize,
    modifier_group_count: usize,
) -> Style {
    Style::default()
        .fg(theme.get_ramp_color(modifier_group_index, modifier_group_count))
        .add_modifier(Modifier::BOLD)
}

/// A group's key block: light text on the group's ramp stop.
fn compute_ramp_key_style(
    theme: &Theme,
    modifier_group_index: usize,
    modifier_group_count: usize,
) -> Style {
    Style::default()
        .fg(theme.ramp_block_text_color)
        .bg(theme.get_ramp_color(modifier_group_index, modifier_group_count))
        .add_modifier(Modifier::BOLD)
}

/// A group's action-label block: the same stop dimmed, quiet text.
fn compute_ramp_label_style(
    theme: &Theme,
    modifier_group_index: usize,
    modifier_group_count: usize,
) -> Style {
    Style::default()
        .fg(theme.dimmed_ramp_text_color)
        .bg(theme.get_dimmed_ramp_color(modifier_group_index, modifier_group_count))
}

/// The pressed-prefix breadcrumb's modifier text: accent on the bar.
fn compute_breadcrumb_modifier_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.accent_color)
        .add_modifier(Modifier::BOLD)
}

/// The pressed-prefix breadcrumb's key/label blocks: dark text on the accent.
fn compute_breadcrumb_key_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.accent_block_text_color)
        .bg(theme.accent_color)
        .add_modifier(Modifier::BOLD)
}

fn compute_breadcrumb_arrow_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.accent_color)
        .add_modifier(Modifier::BOLD)
}

/// The `…` marking hints dropped for width.
fn compute_overflow_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.dimmed_ramp_text_color)
        .add_modifier(Modifier::BOLD)
}

fn compute_revert_marker_style() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(Color::Red)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests;
