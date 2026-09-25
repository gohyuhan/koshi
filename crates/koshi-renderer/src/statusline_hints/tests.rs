//! Tests for the statusline: idle grouping (leaf build_keymap_hints, labeled
//! default prefix groups, `+N` fallbacks once a user build_hint_binding or removal touches
//! a group), the pending_key_sequence-sequence face (breadcrumb plus continuations, nested
//! groups), pinned-first ordering, whole-item truncation, the right-aligned
//! keymap-revert marker, the blanked row for a mode with nothing to hint, and
//! the cells outside the given area that the bar leaves untouched.

use super::*;

use std::collections::BTreeSet;
use std::sync::Arc;

use koshi_core::key::{Key, KeySequence, ModFlags, NamedKey};
use ratatui::buffer::Cell;

use crate::snapshot::HintBinding;

/// A `Ctrl`-modified character chord.
fn build_ctrl_chord(key_character: char) -> KeyChord {
    KeyChord::from_parts(ModFlags::CTRL, Key::Char(key_character))
}

/// An unmodified character chord.
fn build_plain_chord(key_character: char) -> KeyChord {
    KeyChord::from_parts(ModFlags::NONE, Key::Char(key_character))
}

/// A sequence from chords in press order.
fn build_key_sequence(key_chords: &[KeyChord]) -> KeySequence {
    KeySequence::from_first_and_rest(key_chords[0], key_chords[1..].to_vec())
}

/// Create a hint binding with the given flags.
fn build_hint_binding(
    key_sequence: KeySequence,
    action_label: &str,
    is_user_authored: bool,
    is_pinned: bool,
) -> HintBinding {
    HintBinding {
        key_sequence,
        action_display_name: action_label.to_string(),
        is_user_authored,
        is_pinned,
    }
}

/// Assemble a [`KeymapHints`] from its parts.
fn build_keymap_hints(
    hint_bindings: Vec<HintBinding>,
    prefix_labels: &[(KeyChord, &str)],
    removed_key_sequences: Vec<KeySequence>,
    is_reverted_to_defaults: bool,
) -> KeymapHints {
    KeymapHints {
        hint_bindings: Arc::new(hint_bindings),
        prefix_labels: Arc::new(
            prefix_labels
                .iter()
                .map(|(chord, label)| (*chord, (*label).to_string()))
                .collect(),
        ),
        removed_key_sequences: Arc::new(removed_key_sequences.into_iter().collect::<BTreeSet<_>>()),
        is_reverted_to_defaults,
    }
}

/// Draw the bar into a fresh one-row buffer of `column_count` cells.
fn render_statusline(keymap_hints: &KeymapHints, column_count: u16) -> Buffer {
    render_statusline_with_theme(keymap_hints, &Theme::default(), column_count)
}

/// Paint `keymap_hints` into `render_buffer` over `render_area`, in `theme`'s colors. `pending_key_sequence` carries
/// the chords already pressed of an open key sequence, and is `None` when no
/// sequence is open.
fn paint_statusline(
    keymap_hints: &KeymapHints,
    theme: &Theme,
    pending_key_sequence: Option<&KeySequence>,
    render_area: RatatuiRect,
    render_buffer: &mut Buffer,
) {
    draw_statusline(
        StatuslineInputs {
            keymap_hints,
            pending_key_sequence,
            placement_status: None,
        },
        theme,
        render_area,
        render_buffer,
    );
}

/// Draw in `theme`'s colors with an open sequence.
fn render_pending_statusline_with_theme(
    keymap_hints: &KeymapHints,
    theme: &Theme,
    pending_key_sequence: &KeySequence,
    column_count: u16,
) -> Buffer {
    let render_area = RatatuiRect {
        x: 0,
        y: 0,
        width: column_count,
        height: 1,
    };
    let mut render_buffer = Buffer::empty(render_area);
    paint_statusline(
        keymap_hints,
        theme,
        Some(pending_key_sequence),
        render_area,
        &mut render_buffer,
    );
    render_buffer
}

/// Draw with an open sequence, which the viewer owns and hands to the bar.
fn render_pending_statusline(
    keymap_hints: &KeymapHints,
    pending_key_sequence: &KeySequence,
    column_count: u16,
) -> Buffer {
    render_pending_statusline_with_theme(
        keymap_hints,
        &Theme::default(),
        pending_key_sequence,
        column_count,
    )
}

/// Paint the statusline in `theme`'s colors, for the tests that check which
/// color a piece of the bar takes.
fn render_statusline_with_theme(
    keymap_hints: &KeymapHints,
    theme: &Theme,
    column_count: u16,
) -> Buffer {
    let render_area = RatatuiRect {
        x: 0,
        y: 0,
        width: column_count,
        height: 1,
    };
    let mut render_buffer = Buffer::empty(render_area);
    paint_statusline(keymap_hints, theme, None, render_area, &mut render_buffer);
    render_buffer
}

/// The buffer's single row as a string, trailing spaces trimmed.
fn format_rendered_row(render_buffer: &Buffer) -> String {
    let rendered_row_text: String = (0..render_buffer.area.width)
        .map(|column_index| render_buffer[(column_index, 0)].symbol().to_string())
        .collect::<Vec<_>>()
        .join("");
    rendered_row_text.trim_end().to_string()
}

/// One cell per `char` of `text`, each carrying that char and `style`.
///
/// `build_painted_cells("ab", Style::default().fg(Color::Reset))` gives two cells whose
/// symbols are `a` and `b`.
fn build_painted_cells(text: &str, style: Style) -> Vec<Cell> {
    text.chars()
        .map(|character| {
            let mut cell = Cell::default();
            cell.set_char(character);
            cell.set_style(style);
            cell
        })
        .collect()
}

/// The default-shaped fixture: two sequences under `<C-p>` labeled `PANE`,
/// plus a single-chord `Lock` binding.
fn build_pane_keymap_hints(user_close: bool) -> KeymapHints {
    build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('l')]),
                "Lock",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('p'), build_plain_chord('n')]),
                "New Pane",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('p'), build_plain_chord('x')]),
                "Close Pane",
                user_close,
                false,
            ),
        ],
        &[(build_ctrl_chord('p'), "PANE")],
        Vec::new(),
        false,
    )
}

#[test]
fn idle_shows_leaf_hints_and_labeled_default_group() {
    let keymap_hints = build_pane_keymap_hints(false);
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  l  Lock  p  PANE"
    );
}

#[test]
fn modifier_key_and_action_ribbons_use_the_group_ramp_stop() {
    let keymap_hints = build_pane_keymap_hints(false);
    let render_buffer = render_statusline(&keymap_hints, 80);
    // One modifier group → the ramp's purple end everywhere in it: the
    // header as text color, the key block as background, the label block as
    // the dimmed background.
    let purple = Color::Rgb(0xd0, 0xa5, 0xff);
    let purple_dim = Color::Rgb(0x72, 0x5a, 0x8c);
    assert_eq!(render_buffer[(1, 0)].fg, purple);
    assert!(render_buffer[(1, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(render_buffer[(9, 0)].bg, purple);
    assert_eq!(render_buffer[(9, 0)].fg, Color::Rgb(0x12, 0x09, 0x1f));
    assert_eq!(render_buffer[(12, 0)].bg, purple_dim);
    assert_eq!(render_buffer[(12, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
}

#[test]
fn every_cell_of_the_hint_row_is_painted_the_same_way() {
    let render_buffer = render_statusline(&build_pane_keymap_hints(false), 30);
    let header = Style::default()
        .fg(Color::Rgb(0xd0, 0xa5, 0xff))
        .bg(Color::Rgb(0x00, 0x00, 0x00))
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default()
        .fg(Color::Rgb(0x12, 0x09, 0x1f))
        .bg(Color::Rgb(0xd0, 0xa5, 0xff))
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default()
        .fg(Color::Rgb(0xf0, 0xec, 0xfa))
        .bg(Color::Rgb(0x72, 0x5a, 0x8c));
    let fill_style = Style::default().bg(Color::Rgb(0x00, 0x00, 0x00));
    let expected_hint_cells: Vec<Cell> = [
        build_painted_cells(" Ctrl + ", header),
        build_painted_cells(" l ", key_style),
        build_painted_cells(" Lock ", label_style),
        build_painted_cells(" p ", key_style),
        build_painted_cells(" PANE ", label_style),
        build_painted_cells("    ", fill_style),
    ]
    .concat();
    assert_eq!(expected_hint_cells.len(), 30);
    for (column_index, expected_hint_cell) in expected_hint_cells.iter().enumerate() {
        assert_eq!(
            render_buffer[(column_index as u16, 0)],
            *expected_hint_cell,
            "col {column_index}"
        );
    }
}

#[test]
fn human_modifier_groups_fold_same_action_keys() {
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(
                    ModFlags::CTRL,
                    Key::Named(NamedKey::Left),
                )]),
                "Focus Pane",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(
                    ModFlags::CTRL,
                    Key::Named(NamedKey::Down),
                )]),
                "Focus Pane",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(ModFlags::ALT, Key::Char('h'))]),
                "Focus Pane",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(ModFlags::ALT, Key::Char('j'))]),
                "Focus Pane",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    let keymap_hints = keymap;
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  ←↓  Focus Pane  Alt +  hj  Focus Pane"
    );
}

#[test]
fn bare_key_wears_the_header_style_not_a_key_block() {
    let shift_tab = KeyChord::from_parts(ModFlags::SHIFT, Key::Named(NamedKey::Tab));
    let bare_tab = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Tab));
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('l')]),
                "Lock",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[shift_tab]),
                "Previous Tab",
                false,
                false,
            ),
            build_hint_binding(build_key_sequence(&[bare_tab]), "Next Tab", false, false),
        ],
        &[],
        Vec::new(),
        false,
    );
    let keymap_hints = keymap;
    let render_buffer = render_statusline(&keymap_hints, 80);
    assert_eq!(
        format_rendered_row(&render_buffer),
        " Ctrl +  l  Lock  Shift +  Tab  Previous Tab  Tab  Next Tab"
    );
    // The Shift group's key is a block: dark text on the mid ramp stop.
    assert_eq!(render_buffer[(27, 0)].bg, Color::Rgb(0xa7, 0xb0, 0xff));
    assert_eq!(render_buffer[(27, 0)].fg, Color::Rgb(0x12, 0x09, 0x1f));
    // The bare Tab is its own opener: header-styled text on the bar itself —
    // the blue ramp end as foreground over the bar background, no block
    // behind it.
    assert_eq!(render_buffer[(46, 0)].fg, Color::Rgb(0x7d, 0xbc, 0xff));
    assert_eq!(render_buffer[(46, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert!(render_buffer[(46, 0)].modifier.contains(Modifier::BOLD));
    // Its action label keeps the dimmed block, same as any other ribbon.
    assert_eq!(render_buffer[(51, 0)].bg, Color::Rgb(0x44, 0x67, 0x8c));
}

#[test]
fn arrow_keys_sort_left_down_up_right_ahead_of_other_keys() {
    let arrow = |named| {
        build_hint_binding(
            build_key_sequence(&[KeyChord::from_parts(ModFlags::CTRL, Key::Named(named))]),
            "Focus Pane",
            false,
            false,
        )
    };
    let keymap = build_keymap_hints(
        vec![
            arrow(NamedKey::Right),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('z')]),
                "Focus Pane",
                false,
                false,
            ),
            arrow(NamedKey::Up),
            arrow(NamedKey::Left),
            arrow(NamedKey::Down),
        ],
        &[],
        Vec::new(),
        false,
    );
    // One action, so all five keys fold into one ribbon: the four arrows read
    // in screen order, then every other key.
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap, 80)),
        " Ctrl +  ←↓↑→z  Focus Pane"
    );
}

#[test]
fn named_keys_read_as_their_own_names() {
    let create_named_hint_binding = |key, action_label: &str| {
        build_hint_binding(
            build_key_sequence(&[KeyChord::from_parts(ModFlags::NONE, Key::Named(key))]),
            action_label,
            false,
            false,
        )
    };
    let keymap = build_keymap_hints(
        vec![
            create_named_hint_binding(NamedKey::Enter, "Accept"),
            create_named_hint_binding(NamedKey::Esc, "Cancel"),
            create_named_hint_binding(NamedKey::Space, "Pick"),
            create_named_hint_binding(NamedKey::Backspace, "Undo"),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap, 80)),
        " BACKSPACE  Undo  ENTER  Accept  ESC  Cancel  SPACE  Pick"
    );
}

#[test]
fn user_entry_under_prefix_swaps_label_for_count() {
    let keymap_hints = build_pane_keymap_hints(true);
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  l  Lock  p  +2"
    );
}

#[test]
fn removal_under_prefix_swaps_label_for_count() {
    let keymap = build_keymap_hints(
        vec![build_hint_binding(
            build_key_sequence(&[build_ctrl_chord('p'), build_plain_chord('n')]),
            "New Pane",
            false,
            false,
        )],
        &[(build_ctrl_chord('p'), "PANE")],
        vec![build_key_sequence(&[
            build_ctrl_chord('p'),
            build_plain_chord('x'),
        ])],
        false,
    );
    let keymap_hints = keymap;
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  p  +1"
    );
}

#[test]
fn unlabeled_group_shows_count() {
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('t'), build_plain_chord('n')]),
                "New Tab",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('t'), build_plain_chord('x')]),
                "Close Tab",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    let keymap_hints = keymap;
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  t  +2"
    );
}

#[test]
fn pending_prefix_shows_breadcrumb_and_continuations() {
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    let keymap_hints = build_pane_keymap_hints(false);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            80,
        )),
        " Ctrl +  p  PANE  ▶  n  New Pane  x  Close Pane"
    );
}

#[test]
fn pending_prefix_with_no_continuations_shows_bare_breadcrumb_and_no_groups() {
    // The user pressed a chord that isn't a prefix of anything bound: no
    // matching entries mean no label and no continuation groups — just the
    // breadcrumb and arrow, with no panic on the now-empty group list.
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('z')]);
    let keymap_hints = build_pane_keymap_hints(false);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            80,
        )),
        " Ctrl +  z  ▶"
    );
}

#[test]
fn customized_pending_prefix_uses_count_not_shipped_label() {
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    let keymap_hints = build_pane_keymap_hints(true);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            80,
        )),
        " Ctrl +  p  +2  ▶  n  New Pane  x  Close Pane"
    );
}

#[test]
fn pending_prefix_without_label_shows_derived_count() {
    let keymap = build_keymap_hints(
        vec![build_hint_binding(
            build_key_sequence(&[build_ctrl_chord('t'), build_plain_chord('n')]),
            "New Tab",
            false,
            false,
        )],
        &[],
        Vec::new(),
        false,
    );
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('t')]);
    let keymap_hints = keymap;
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            80,
        )),
        " Ctrl +  t  +1  ▶  n  New Tab"
    );
}

#[test]
fn nested_group_inside_pending_shows_count() {
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[
                    build_ctrl_chord('p'),
                    build_plain_chord('n'),
                    build_plain_chord('a'),
                ]),
                "Deep A",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[
                    build_ctrl_chord('p'),
                    build_plain_chord('n'),
                    build_plain_chord('b'),
                ]),
                "Deep B",
                false,
                false,
            ),
        ],
        &[(build_ctrl_chord('p'), "PANE")],
        Vec::new(),
        false,
    );
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    let bar = keymap;
    assert_eq!(
        format_rendered_row(&render_pending_statusline(&bar, &pending_key_sequence, 80)),
        " Ctrl +  p  PANE  ▶  n  +2"
    );
}

#[test]
fn chord_bound_and_extended_shows_action_with_count() {
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('p')]),
                "Pane Menu",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('p'), build_plain_chord('n')]),
                "New Pane",
                false,
                false,
            ),
        ],
        &[(build_ctrl_chord('p'), "PANE")],
        Vec::new(),
        false,
    );
    let keymap_hints = keymap;
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  p  Pane Menu +1"
    );
}

#[test]
fn pinned_hint_sorts_first_and_survives_truncation() {
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('a')]),
                "Aardvark",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('g')]),
                "Unlock",
                false,
                true,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    let bar = keymap;
    // Wide: pinned first despite `<C-a>` sorting lower.
    assert_eq!(
        format_rendered_row(&render_statusline(&bar, 80)),
        " Ctrl +  g  Unlock  a  Aardvark"
    );
    // Narrow: only the pinned hint fits; the dropped one leaves a `…`.
    assert_eq!(
        format_rendered_row(&render_statusline(&bar, 19)),
        " Ctrl +  g  Unlock…"
    );
}

#[test]
fn a_pinned_hint_does_not_pull_its_modifier_group_ahead() {
    // Pinned puts a hint first inside its own group; the groups themselves
    // still read in modifier order, so `Ctrl` leads the pinned `Alt` hint.
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('l')]),
                "Lock",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(ModFlags::ALT, Key::Char('u'))]),
                "Unlock",
                false,
                true,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap, 80)),
        " Ctrl +  l  Lock  Alt +  u  Unlock"
    );
}

#[test]
fn a_pinned_and_an_unpinned_hint_with_one_label_stay_two_ribbons() {
    // Folding keys into one ribbon needs the same label *and* the same pinned
    // flag, so these two keep their own blocks with the pinned one first.
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('w')]),
                "Save",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('s')]),
                "Save",
                false,
                true,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap, 80)),
        " Ctrl +  s  Save  w  Save"
    );
}

#[test]
fn truncation_drops_whole_trailing_hints() {
    let keymap_hints = build_pane_keymap_hints(false);
    // Shared `Ctrl +` header plus the first ribbon is 17 cells; the second
    // ribbon needs 9 more, so below 26 it is dropped whole behind a `…`.
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 25)),
        " Ctrl +  l  Lock …"
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 26)),
        " Ctrl +  l  Lock  p  PANE"
    );
}

#[test]
fn a_group_header_is_never_painted_without_its_first_ribbon() {
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('l')]),
                "Lock",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(ModFlags::ALT, Key::Char('u'))]),
                "Unlock",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    // The `Ctrl` group is 17 cells, the `Alt` group's ` Alt + ` header plus its
    // first ribbon is 18 more. One cell short of both, the header is skipped
    // whole rather than build_painted_cells over an empty group.
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap, 34)),
        " Ctrl +  l  Lock …"
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap, 35)),
        " Ctrl +  l  Lock  Alt +  u  Unlock"
    );
}

#[test]
fn a_breadcrumb_with_no_room_for_the_arrow_ends_in_the_overflow_marker() {
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    let keymap_hints = build_pane_keymap_hints(false);
    // The breadcrumb ` Ctrl +  p  PANE ` is 17 cells and the ` ▶ ` arrow 3
    // more. At 17 the arrow is dropped and the `…` takes the breadcrumb's last
    // cell; at 20 both fit and the `…` stands for the dropped hint groups.
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            17,
        )),
        " Ctrl +  p  PANE…"
    );
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            20,
        )),
        " Ctrl +  p  PANE  ▶…"
    );
}

#[test]
fn an_overflow_marker_with_no_cell_left_takes_the_last_one() {
    let keymap_hints = build_pane_keymap_hints(false);
    // The `Ctrl +` header plus the first ribbon fill all 17 cells exactly, so
    // the `…` standing for the dropped second ribbon has no cell of its own and
    // overwrites the last cell of the ribbon before it.
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 17)),
        " Ctrl +  l  Lock…"
    );
}

#[test]
fn the_overflow_marker_is_bold_dim_ramp_text_on_the_bar_background() {
    let render_buffer = render_statusline(&build_pane_keymap_hints(false), 25);
    // Column 17 is the first cell past the ` Ctrl +  l  Lock ` group, so the
    // `…` there lands on bar background rather than on a ribbon.
    assert_eq!(render_buffer[(17, 0)].symbol(), "…");
    assert_eq!(render_buffer[(17, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
    assert_eq!(render_buffer[(17, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert!(render_buffer[(17, 0)].modifier.contains(Modifier::BOLD));
}

#[test]
fn the_revert_marker_is_bold_white_on_red() {
    let keymap = KeymapHints {
        is_reverted_to_defaults: true,
        ..build_pane_keymap_hints(false)
    };
    let render_buffer = render_statusline(&keymap, 30);
    assert_eq!(render_buffer[(24, 0)].symbol(), "k");
    assert_eq!(render_buffer[(24, 0)].fg, Color::White);
    assert_eq!(render_buffer[(24, 0)].bg, Color::Red);
    assert!(render_buffer[(24, 0)].modifier.contains(Modifier::BOLD));
}

#[test]
fn a_named_key_with_no_symbol_reads_as_its_chord_spelling() {
    let create_named_hint_binding = |key, action_label: &str| {
        build_hint_binding(
            build_key_sequence(&[KeyChord::from_parts(ModFlags::NONE, Key::Named(key))]),
            action_label,
            false,
            false,
        )
    };
    let keymap = build_keymap_hints(
        vec![
            create_named_hint_binding(NamedKey::Home, "Top"),
            create_named_hint_binding(NamedKey::End, "Bottom"),
            create_named_hint_binding(NamedKey::PageUp, "Page Up"),
            create_named_hint_binding(NamedKey::Delete, "Delete"),
            create_named_hint_binding(NamedKey::F(1), "Help"),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap, 80)),
        " Del  Delete  End  Bottom  F1  Help  Home  Top  PageUp  Page Up"
    );
}

#[test]
fn the_bar_paints_only_the_cells_of_the_area_it_is_given() {
    let keymap_hints = build_keymap_hints(
        vec![build_hint_binding(
            build_key_sequence(&[build_plain_chord('q')]),
            "Go",
            false,
            false,
        )],
        &[],
        Vec::new(),
        false,
    );
    let buf_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 3,
    };
    let mut render_buffer = Buffer::empty(buf_area);
    for row_index in 0..3 {
        render_buffer.set_string(0, row_index, "X".repeat(20), Style::default());
    }

    paint_statusline(
        &keymap_hints,
        &Theme::default(),
        None,
        RatatuiRect {
            x: 3,
            y: 1,
            width: 12,
            height: 1,
        },
        &mut render_buffer,
    );

    let format_row_text = |row_index: u16| -> String {
        (0..20)
            .map(|column_index| {
                render_buffer[(column_index, row_index)]
                    .symbol()
                    .to_string()
            })
            .collect()
    };
    assert_eq!(format_row_text(0), "X".repeat(20));
    assert_eq!(format_row_text(1), "XXX q  Go      XXXXX");
    assert_eq!(format_row_text(2), "X".repeat(20));
}

#[test]
fn a_removal_under_a_pending_prefix_swaps_every_label_it_touches_for_a_count() {
    let create_hint_bindings = || {
        vec![build_hint_binding(
            build_key_sequence(&[
                build_ctrl_chord('p'),
                build_plain_chord('n'),
                build_plain_chord('a'),
            ]),
            "Deep A",
            false,
            false,
        )]
    };
    let prefix_labels: &[(KeyChord, &str)] = &[
        (build_ctrl_chord('p'), "PANE"),
        (build_plain_chord('n'), "NESTED"),
    ];
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);

    let untouched = build_keymap_hints(create_hint_bindings(), prefix_labels, Vec::new(), false);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &untouched,
            &pending_key_sequence,
            80
        )),
        " Ctrl +  p  PANE  ▶  n  NESTED"
    );

    // `<C-p> n b` was removed. The removal sits under `<C-p>` and under
    // `<C-p> n`, so both prefix labels give way to their build_hint_binding counts.
    let with_removal = build_keymap_hints(
        create_hint_bindings(),
        prefix_labels,
        vec![build_key_sequence(&[
            build_ctrl_chord('p'),
            build_plain_chord('n'),
            build_plain_chord('b'),
        ])],
        false,
    );
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &with_removal,
            &pending_key_sequence,
            80
        )),
        " Ctrl +  p  +1  ▶  n  +1"
    );
}

#[test]
fn revert_marker_holds_right_edge_and_hints_stop_short() {
    let keymap = KeymapHints {
        is_reverted_to_defaults: true,
        ..build_pane_keymap_hints(false)
    };
    let keymap_hints = keymap;
    let render_buffer = render_statusline(&keymap_hints, 30);
    let rendered_row_text = format_rendered_row(&render_buffer);
    assert_eq!(rendered_row_text, " Ctrl +  l  Lock …      keys!");
    // Marker text holds the right edge, with one background-padding cell.
    assert_eq!(render_buffer[(28, 0)].symbol(), "!");
}

#[test]
fn modifier_groups_read_ctrl_alt_ctrl_shift_shift_super_then_the_rest() {
    let create_modifier_hint_binding = |modifier_flags, key_character: char, action_label: &str| {
        build_hint_binding(
            build_key_sequence(&[KeyChord::from_parts(
                modifier_flags,
                Key::Char(key_character),
            )]),
            action_label,
            false,
            false,
        )
    };
    // Fed in reverse of the order they must come out in.
    let keymap = build_keymap_hints(
        vec![
            create_modifier_hint_binding(ModFlags::CTRL | ModFlags::ALT, 'f', "CtrlAlt"),
            create_modifier_hint_binding(ModFlags::NONE, 'g', "Bare"),
            create_modifier_hint_binding(ModFlags::SUPER, 'a', "Super"),
            create_modifier_hint_binding(ModFlags::SHIFT, 'b', "Shift"),
            create_modifier_hint_binding(ModFlags::CTRL | ModFlags::SHIFT, 'c', "CtrlShift"),
            create_modifier_hint_binding(ModFlags::ALT, 'd', "Alt"),
            create_modifier_hint_binding(ModFlags::CTRL, 'e', "Ctrl"),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap, 200)),
        concat!(
            " Ctrl +  e  Ctrl  Alt +  d  Alt  Ctrl+Shift +  c  CtrlShift ",
            " Shift +  b  Shift  Super +  a  Super  g  Bare  Ctrl+Alt +  f  CtrlAlt"
        )
    );
}

#[test]
fn only_the_opening_chord_of_a_pending_sequence_shows_a_prefix_label() {
    // `n` is a labeled top-level prefix in its own right, but here it is the
    // second chord of the open sequence, so its `NESTED` label stays off the
    // breadcrumb — only the chord that opened the sequence is labeled.
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[
                    build_ctrl_chord('p'),
                    build_plain_chord('n'),
                    build_plain_chord('a'),
                ]),
                "Deep A",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_plain_chord('n'), build_plain_chord('z')]),
                "Other",
                false,
                false,
            ),
        ],
        &[
            (build_ctrl_chord('p'), "PANE"),
            (build_plain_chord('n'), "NESTED"),
        ],
        Vec::new(),
        false,
    );
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p'), build_plain_chord('n')]);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap,
            &pending_key_sequence,
            80
        )),
        " Ctrl +  p  PANE  n  ▶  a  Deep A"
    );
}

#[test]
fn a_pending_sequence_that_is_itself_bound_lists_only_its_continuations() {
    // `<C-p>` runs an action of its own and also opens deeper bindings. Once it
    // is pending_key_sequence, its own build_hint_binding is behind the viewer, so only `n` is listed.
    let keymap = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('p')]),
                "Pane Menu",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('p'), build_plain_chord('n')]),
                "New Pane",
                false,
                false,
            ),
        ],
        &[(build_ctrl_chord('p'), "PANE")],
        Vec::new(),
        false,
    );
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap,
            &pending_key_sequence,
            80
        )),
        " Ctrl +  p  PANE  ▶  n  New Pane"
    );
}

#[test]
fn a_row_too_narrow_for_the_breadcrumb_shows_only_the_overflow_marker() {
    // The breadcrumb ribbon ` Ctrl +  p  PANE ` needs 17 cells; at 10 nothing
    // of it fits, so the row is just the `…`.
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    let keymap_hints = build_pane_keymap_hints(false);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            10,
        )),
        "…"
    );
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            1,
        )),
        "…"
    );
}

#[test]
fn the_revert_marker_and_the_overflow_marker_share_the_narrowest_row_that_fits_both() {
    // ` keys! ` is 7 cells; at 8 the marker takes the right edge and the `…`
    // for every dropped hint takes the one cell left of it.
    let keymap = KeymapHints {
        is_reverted_to_defaults: true,
        ..build_pane_keymap_hints(false)
    };
    let render_buffer = render_statusline(&keymap, 8);
    assert_eq!(format_rendered_row(&render_buffer), "… keys!");
    assert_eq!(render_buffer[(0, 0)].symbol(), "…");
    assert_eq!(render_buffer[(7, 0)].symbol(), " ");
}

#[test]
fn a_zero_height_area_leaves_the_row_untouched() {
    let keymap_hints = build_pane_keymap_hints(false);
    let render_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 10,
        height: 0,
    };
    let mut render_buffer = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 10,
        height: 1,
    });
    render_buffer.set_string(0, 0, "X".repeat(10), Style::default());
    paint_statusline(
        &keymap_hints,
        &Theme::default(),
        None,
        render_area,
        &mut render_buffer,
    );
    assert_eq!(format_rendered_row(&render_buffer), "XXXXXXXXXX");
}

#[test]
fn empty_mode_blanks_the_row() {
    let keymap_hints = build_keymap_hints(Vec::new(), &[], Vec::new(), false);
    let render_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 1,
    };
    let mut render_buffer = Buffer::empty(render_area);
    // Pre-fill the row: the bar owns it, so stale cells must be cleared.
    render_buffer.set_string(0, 0, "X".repeat(20), Style::default());
    paint_statusline(
        &keymap_hints,
        &Theme::default(),
        None,
        render_area,
        &mut render_buffer,
    );
    assert_eq!(format_rendered_row(&render_buffer), "");
    // Blank of text, but not of color: the row still carries the bar
    // background, so an empty mode reads as a bar rather than a hole.
    for column_index in 0..20 {
        assert_eq!(
            render_buffer[(column_index, 0)].bg,
            Color::Rgb(0x00, 0x00, 0x00),
            "col {column_index}"
        );
    }
}

#[test]
fn zero_size_area_draws_nothing() {
    let keymap_hints = build_pane_keymap_hints(false);
    let render_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };
    let mut render_buffer = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 10,
        height: 1,
    });
    paint_statusline(
        &keymap_hints,
        &Theme::default(),
        None,
        render_area,
        &mut render_buffer,
    );
    assert_eq!(format_rendered_row(&render_buffer), "");
}

/// A non-default palette recolors the bar: the pending_key_sequence breadcrumb takes the
/// theme's accent pair and a group's key block sits on the custom ramp.
#[test]
fn a_custom_theme_recolors_the_bar() {
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    let keymap_hints = build_pane_keymap_hints(false);
    let render_theme = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ramp_end: (0x00, 0x00, 0xff),
        accent_color: Color::Rgb(0x00, 0xff, 0x00),
        accent_block_text_color: Color::Rgb(0x01, 0x02, 0x03),
        ..Theme::default()
    };
    let render_buffer = render_pending_statusline_with_theme(
        &keymap_hints,
        &render_theme,
        &pending_key_sequence,
        80,
    );
    // Row: " Ctrl +  p  PANE  ▶  n  New Pane …". The breadcrumb's `Ctrl +`
    // is accent text; its key block is on-accent text on the accent.
    assert_eq!(render_buffer[(1, 0)].fg, Color::Rgb(0x00, 0xff, 0x00));
    assert_eq!(render_buffer[(9, 0)].fg, Color::Rgb(0x01, 0x02, 0x03));
    assert_eq!(render_buffer[(9, 0)].bg, Color::Rgb(0x00, 0xff, 0x00));
    // The modifier-less continuation key wears the group's header style: the
    // custom ramp's start stop as its text color.
    let continuation_column = (0..80)
        .find(|&column_index| render_buffer[(column_index, 0)].symbol() == "n")
        .expect("continuation key drawn");
    assert_eq!(
        render_buffer[(continuation_column, 0)].fg,
        Color::Rgb(0xff, 0x00, 0x00)
    );
}

#[test]
fn the_overflow_marker_never_paints_left_of_the_area() {
    // The revert marker fills the bar and pulls the right edge down to the
    // area's own left edge, so the overflow marker has nowhere inside to go.
    let keymap_hints = KeymapHints {
        is_reverted_to_defaults: true,
        ..build_pane_keymap_hints(false)
    };
    let mut render_buffer = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 1,
    });
    render_buffer.set_string(0, 0, "X".repeat(20), Style::default());

    paint_statusline(
        &keymap_hints,
        &Theme::default(),
        None,
        RatatuiRect {
            x: 5,
            y: 0,
            width: 6,
            height: 1,
        },
        &mut render_buffer,
    );

    let outside_area_text: String = (0..5)
        .map(|column_index| render_buffer[(column_index, 0)].symbol().to_string())
        .collect();
    assert_eq!(
        outside_area_text, "XXXXX",
        "columns 0 to 4 belong to the caller"
    );
}

#[test]
fn a_hint_wider_than_the_cell_counter_is_dropped_behind_the_overflow_marker() {
    // A ribbon of 65 536 cells reads as 65 535, which never fits, instead of
    // wrapping to 0 and painting nothing where the marker belongs.
    let oversized_action_label = "L".repeat(65_536 - 5);
    let keymap_hints = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_plain_chord('a')]),
                &oversized_action_label,
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_plain_chord('b')]),
                "Second",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );

    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 40)),
        "…"
    );
}

#[test]
fn placement_statusline_reserves_the_right_edge_and_styles_each_status_kind() {
    let keymap_hints = build_keymap_hints(Vec::new(), &[], Vec::new(), false);
    let status_text = "PLACE D | swap with B";
    let valid_placement_status = PlacementStatus {
        placement_status_kind: PlacementStatusKind::Valid,
        status_text: status_text.to_string(),
    };
    let invalid_placement_status = PlacementStatus {
        placement_status_kind: PlacementStatusKind::Invalid,
        status_text: "PLACE D | choose a destination".to_string(),
    };
    let loading_placement_status = PlacementStatus {
        placement_status_kind: PlacementStatusKind::Loading,
        status_text: "PLACE D | loading logs".to_string(),
    };

    let render_status = |placement_status: &PlacementStatus| {
        let render_area = RatatuiRect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        let mut render_buffer = Buffer::empty(render_area);
        draw_statusline(
            StatuslineInputs {
                keymap_hints: &keymap_hints,
                pending_key_sequence: None,
                placement_status: Some(placement_status),
            },
            &Theme::default(),
            render_area,
            &mut render_buffer,
        );
        render_buffer
    };

    let valid_render_buffer = render_status(&valid_placement_status);
    let invalid_render_buffer = render_status(&invalid_placement_status);
    let loading_render_buffer = render_status(&loading_placement_status);
    assert_eq!(
        format_rendered_row(&valid_render_buffer),
        format!("{:>80}", format!(" {status_text} ")).trim_end()
    );
    let valid_status_start_column = 80 - (status_text.chars().count() as u16 + 2);
    assert_eq!(
        valid_render_buffer[(valid_status_start_column, 0)].symbol(),
        " "
    );
    assert_ne!(
        valid_render_buffer[(79, 0)].bg,
        invalid_render_buffer[(79, 0)].bg
    );
    assert_ne!(
        invalid_render_buffer[(79, 0)].fg,
        loading_render_buffer[(79, 0)].fg
    );
}
