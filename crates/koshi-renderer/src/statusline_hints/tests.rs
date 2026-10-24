//! Tests for the statusline: idle grouping (leaf hints, labeled default prefix
//! groups, `+N` counts once a user binding or a removal touches a group), the
//! key names of a folded ribbon (`←↓`, `hj`, `ENTER|f`, and the shipped pane
//! move chooser row), the open-sequence view (breadcrumb plus continuations,
//! nested groups), group order, pinned-first and hint-tier ordering,
//! whole-item truncation, the shipped rows of every mode and of the `<C-p>`
//! continuations at 40, 80 and 120 columns, the blanked row for a mode with
//! nothing to hint, and the cells outside the given area that the bar leaves
//! untouched.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use koshi_config::conflict::{build_keymap_layers, detect_conflicts, KeymapVerdict};
use koshi_config::hints::KeymapHintCatalog;
use koshi_config::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::action::{ActionReference, HintTier};
use koshi_core::key::{BindingModifierFlags, Key, KeySequence, NamedKey};
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;
use ratatui::buffer::Cell;

use crate::snapshot::HintBinding;

/// A `Ctrl`-modified character chord.
fn build_ctrl_chord(key_character: char) -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char(key_character))
}

/// An unmodified character chord.
fn build_plain_chord(key_character: char) -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char(key_character))
}

/// A sequence from chords in press order.
fn build_key_sequence(key_chords: &[KeyChord]) -> KeySequence {
    KeySequence::from_first_and_rest(key_chords[0], key_chords[1..].to_vec())
}

/// Create a hint binding with the given flags, in the [`HintTier::Other`]
/// hint tier.
fn build_hint_binding(
    key_sequence: KeySequence,
    action_label: &str,
    is_user_authored: bool,
    is_pinned: bool,
) -> HintBinding {
    HintBinding {
        key_sequence,
        action_label: action_label.to_string(),
        hint_tier: HintTier::Other,
        is_user_authored,
        is_pinned,
    }
}

/// Assemble a [`KeymapHints`] from its parts.
fn build_keymap_hints(
    hint_bindings: Vec<HintBinding>,
    prefix_labels: &[(KeyChord, &str)],
    removed_key_sequences: Vec<KeySequence>,
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
    }
}

/// Draw the bar into a fresh one-row buffer of `column_count` cells, with no
/// open sequence.
fn render_statusline(keymap_hints: &KeymapHints, column_count: u16) -> Buffer {
    render_statusline_row(keymap_hints, &Theme::default(), None, column_count)
}

/// Paint `keymap_hints` into `render_buffer` over `render_area`, in `theme`'s colors.
/// `pending_key_sequence` carries the chords already pressed of an open key sequence, and is `None`
/// when no sequence is open.
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
            is_recovery_notice_visible: false,
        },
        theme,
        render_area,
        render_buffer,
    );
}

#[test]
fn recovery_notice_replaces_other_statusline_content_at_full_and_narrow_widths() {
    let keymap_hints = build_keymap_hints(Vec::new(), &[], Vec::new());
    for (column_count, expected_text) in [
        (
            80,
            "Restore failed: new shell; previous panes unavailable. Input clears notice.",
        ),
        (15, "Restore failed:"),
        (1, "R"),
    ] {
        let statusline_area = RatatuiRect {
            x: 0,
            y: 0,
            width: column_count,
            height: 1,
        };
        let mut screen_buffer = Buffer::empty(statusline_area);
        draw_statusline(
            StatuslineInputs {
                keymap_hints: &keymap_hints,
                pending_key_sequence: None,
                is_recovery_notice_visible: true,
            },
            &Theme::default(),
            statusline_area,
            &mut screen_buffer,
        );
        assert_eq!(format_rendered_row(&screen_buffer), expected_text);
        assert_eq!(screen_buffer[(0, 0)].fg, Color::White);
        assert_eq!(screen_buffer[(0, 0)].bg, Color::Red);
    }
}

/// Draw the bar into a fresh one-row buffer of `column_count` cells, in
/// `theme`'s colors. `pending_key_sequence` is `None` when no sequence is open.
fn render_statusline_row(
    keymap_hints: &KeymapHints,
    theme: &Theme,
    pending_key_sequence: Option<&KeySequence>,
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
        pending_key_sequence,
        render_area,
        &mut render_buffer,
    );
    render_buffer
}

/// Draw the bar into a fresh one-row buffer of `column_count` cells, with
/// `pending_key_sequence` open.
fn render_pending_statusline(
    keymap_hints: &KeymapHints,
    pending_key_sequence: &KeySequence,
    column_count: u16,
) -> Buffer {
    render_statusline_row(
        keymap_hints,
        &Theme::default(),
        Some(pending_key_sequence),
        column_count,
    )
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
fn build_pane_keymap_hints(has_user_close_binding: bool) -> KeymapHints {
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
                has_user_close_binding,
                false,
            ),
        ],
        &[(build_ctrl_chord('p'), "PANE")],
        Vec::new(),
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
    let ramp_start_color = Color::Rgb(0xd0, 0xa5, 0xff);
    let dimmed_ramp_start_color = Color::Rgb(0x72, 0x5a, 0x8c);
    assert_eq!(render_buffer[(1, 0)].fg, ramp_start_color);
    assert!(render_buffer[(1, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(render_buffer[(9, 0)].bg, ramp_start_color);
    assert_eq!(render_buffer[(9, 0)].fg, Color::Rgb(0x12, 0x09, 0x1f));
    assert_eq!(render_buffer[(12, 0)].bg, dimmed_ramp_start_color);
    assert_eq!(render_buffer[(12, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
}

#[test]
fn every_cell_of_the_hint_row_is_painted_the_same_way() {
    let render_buffer = render_statusline(&build_pane_keymap_hints(false), 30);
    let header_style = Style::default()
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
        build_painted_cells(" Ctrl + ", header_style),
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
    let keymap_hints = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(
                    BindingModifierFlags::CTRL,
                    Key::Named(NamedKey::Left),
                )]),
                "Focus Pane",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(
                    BindingModifierFlags::CTRL,
                    Key::Named(NamedKey::Down),
                )]),
                "Focus Pane",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(
                    BindingModifierFlags::ALT,
                    Key::Char('h'),
                )]),
                "Focus Pane",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(
                    BindingModifierFlags::ALT,
                    Key::Char('j'),
                )]),
                "Focus Pane",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  ←↓  Focus Pane  Alt +  hj  Focus Pane"
    );
}

#[test]
fn bare_key_wears_the_header_style_not_a_key_block() {
    let shift_tab = KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Named(NamedKey::Tab));
    let bare_tab = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Tab));
    let keymap_hints = build_keymap_hints(
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
    );
    let render_buffer = render_statusline(&keymap_hints, 80);
    assert_eq!(
        format_rendered_row(&render_buffer),
        " Ctrl +  l  Lock  Tab  Next Tab  Shift +  Tab  Previous Tab"
    );
    // The bare Tab is its own opener: header-styled text on the bar itself —
    // the mid ramp stop as foreground over the bar background, no block
    // behind it.
    assert_eq!(render_buffer[(18, 0)].fg, Color::Rgb(0xa7, 0xb0, 0xff));
    assert_eq!(render_buffer[(18, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert!(render_buffer[(18, 0)].modifier.contains(Modifier::BOLD));
    // Its action label keeps the dimmed block, same as any other ribbon.
    assert_eq!(render_buffer[(23, 0)].bg, Color::Rgb(0x5b, 0x60, 0x8c));
    // The Shift group's key is a block: dark text on the blue ramp end.
    assert_eq!(render_buffer[(42, 0)].bg, Color::Rgb(0x7d, 0xbc, 0xff));
    assert_eq!(render_buffer[(42, 0)].fg, Color::Rgb(0x12, 0x09, 0x1f));
}

#[test]
fn arrow_keys_sort_left_down_up_right_ahead_of_other_keys() {
    let build_arrow_hint_binding = |arrow_key| {
        build_hint_binding(
            build_key_sequence(&[KeyChord::from_parts(
                BindingModifierFlags::CTRL,
                Key::Named(arrow_key),
            )]),
            "Focus Pane",
            false,
            false,
        )
    };
    let keymap_hints = build_keymap_hints(
        vec![
            build_arrow_hint_binding(NamedKey::Right),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('z')]),
                "Focus Pane",
                false,
                false,
            ),
            build_arrow_hint_binding(NamedKey::Up),
            build_arrow_hint_binding(NamedKey::Left),
            build_arrow_hint_binding(NamedKey::Down),
        ],
        &[],
        Vec::new(),
    );
    // One action: all five keys fold into one ribbon, the four arrows in
    // screen order, then every other key.
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  ←↓↑→z  Focus Pane"
    );
}

#[test]
fn keys_in_one_group_read_pinned_then_essential_then_navigation_then_other() {
    let build_tiered_hint_binding =
        |key_character: char, action_label: &str, hint_tier: HintTier, is_pinned: bool| {
            HintBinding {
                hint_tier,
                ..build_hint_binding(
                    build_key_sequence(&[build_ctrl_chord(key_character)]),
                    action_label,
                    false,
                    is_pinned,
                )
            }
        };
    // Fed in reverse of the order they must come out in. Inside one tier,
    // keys read in key order: `c` before `d`.
    let keymap_hints = build_keymap_hints(
        vec![
            build_tiered_hint_binding('a', "Other", HintTier::Other, false),
            build_tiered_hint_binding('b', "Navigation", HintTier::Navigation, false),
            build_tiered_hint_binding('d', "Dog", HintTier::Essential, false),
            build_tiered_hint_binding('c', "Cat", HintTier::Essential, false),
            build_tiered_hint_binding('z', "Pinned", HintTier::Other, true),
        ],
        &[],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  z  Pinned  c  Cat  d  Dog  b  Navigation  a  Other"
    );
}

#[test]
fn a_prefix_key_takes_the_first_tier_among_the_bindings_under_it() {
    // `<C-t>` starts one `Other` and one `Essential` binding, so it reads as
    // `Essential` and leads the `Other` leaf `<C-a>`.
    let keymap_hints = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('a')]),
                "Alpha",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('t'), build_plain_chord('m')]),
                "Move Tab",
                false,
                false,
            ),
            HintBinding {
                hint_tier: HintTier::Essential,
                ..build_hint_binding(
                    build_key_sequence(&[build_ctrl_chord('t'), build_plain_chord('n')]),
                    "New Tab",
                    false,
                    false,
                )
            },
        ],
        &[],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  t  +2  a  Alpha"
    );
}

#[test]
fn named_keys_read_as_their_own_names() {
    let build_named_hint_binding = |named_key, action_label: &str| {
        build_hint_binding(
            build_key_sequence(&[KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Named(named_key),
            )]),
            action_label,
            false,
            false,
        )
    };
    let keymap_hints = build_keymap_hints(
        vec![
            build_named_hint_binding(NamedKey::Enter, "Accept"),
            build_named_hint_binding(NamedKey::Esc, "Cancel"),
            build_named_hint_binding(NamedKey::Space, "Pick"),
            build_named_hint_binding(NamedKey::Backspace, "Undo"),
        ],
        &[],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " BACKSPACE  Undo  ENTER  Accept  ESC  Cancel  SPACE  Pick"
    );
}

#[test]
fn the_pane_move_choosers_80_column_row_separates_enter_from_the_key_it_folds_with() {
    let hint_catalog = KeymapHintCatalog::from_registry(&ActionRegistry::new());
    // Each case: whether the focused pane is floating, then the row the
    // shipped `pane-move-choice` bindings paint.
    for (is_focused_pane_floating, expected_row) in [
        (
            true,
            " ENTER|f  Move Floating Pane  ESC  Cancel Move  t  Move Tiled Pane",
        ),
        (
            false,
            " ENTER|t  Move Tiled Pane  ESC  Cancel Move  f  Move Floating Pane",
        ),
    ] {
        let keymap_hints = hint_catalog.build_frame_hints(
            LockMode::PaneMoveChoice,
            false,
            is_focused_pane_floating,
        );
        assert_eq!(
            format_rendered_row(&render_statusline(&keymap_hints, 80)),
            expected_row,
            "focused pane floating: {is_focused_pane_floating}"
        );
    }
}

#[test]
fn modified_keys_folded_with_a_longer_key_name_are_separated_by_a_bar() {
    let build_ctrl_hint_binding = |key, action_label: &str| {
        build_hint_binding(
            build_key_sequence(&[KeyChord::from_parts(BindingModifierFlags::CTRL, key)]),
            action_label,
            false,
            false,
        )
    };
    let keymap_hints = build_keymap_hints(
        vec![
            build_ctrl_hint_binding(Key::Named(NamedKey::Tab), "Next Tab"),
            build_ctrl_hint_binding(Key::Char('n'), "Next Tab"),
            build_ctrl_hint_binding(Key::Named(NamedKey::F(1)), "Help"),
            build_ctrl_hint_binding(Key::Named(NamedKey::F(2)), "Help"),
        ],
        &[],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  F1|F2  Help  Tab|n  Next Tab"
    );
}

#[test]
fn a_bar_key_folded_with_a_longer_key_name_follows_its_separator() {
    let keymap_hints = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_plain_chord('|')]),
                "Split",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(
                    BindingModifierFlags::NONE,
                    Key::Named(NamedKey::Tab),
                )]),
                "Split",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Tab||  Split"
    );
}

#[test]
fn pending_continuations_folded_with_a_longer_key_name_are_separated_by_a_bar() {
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    let keymap_hints = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[
                    build_ctrl_chord('p'),
                    KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Tab)),
                ]),
                "Next Floating Pane",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('p'), build_plain_chord('n')]),
                "Next Floating Pane",
                false,
                false,
            ),
        ],
        &[(build_ctrl_chord('p'), "PANE")],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            80,
        )),
        " Ctrl +  p  PANE  ▶  Tab|n  Next Floating Pane"
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
    let keymap_hints = build_keymap_hints(
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
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  p  +1"
    );
}

#[test]
fn unlabeled_group_shows_count() {
    let keymap_hints = build_keymap_hints(
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
    );
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
    // A pressed chord that opens nothing bound shows no label and no
    // continuation groups: only the breadcrumb and the arrow.
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
    let keymap_hints = build_keymap_hints(
        vec![build_hint_binding(
            build_key_sequence(&[build_ctrl_chord('t'), build_plain_chord('n')]),
            "New Tab",
            false,
            false,
        )],
        &[],
        Vec::new(),
    );
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('t')]);
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
    let keymap_hints = build_keymap_hints(
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
    );
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            80
        )),
        " Ctrl +  p  PANE  ▶  n  +2"
    );
}

#[test]
fn chord_bound_and_extended_shows_action_with_count() {
    let keymap_hints = build_keymap_hints(
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
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  p  Pane Menu +1"
    );
}

#[test]
fn pinned_hint_sorts_first_and_survives_truncation() {
    let keymap_hints = build_keymap_hints(
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
    );
    // Wide: pinned first, ahead of `<C-a>`.
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Ctrl +  g  Unlock  a  Aardvark"
    );
    // Narrow: only the pinned hint fits; the dropped one leaves a `…`.
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 19)),
        " Ctrl +  g  Unlock…"
    );
}

#[test]
fn a_pinned_hint_pulls_its_modifier_group_ahead() {
    // The `Alt` group holds the pinned `Unlock`, so it reads before `Ctrl`.
    let keymap_hints = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('l')]),
                "Lock",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(
                    BindingModifierFlags::ALT,
                    Key::Char('u'),
                )]),
                "Unlock",
                false,
                true,
            ),
        ],
        &[],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Alt +  u  Unlock  Ctrl +  l  Lock"
    );
}

#[test]
fn a_pinned_and_an_unpinned_hint_with_one_label_stay_two_ribbons() {
    // Keys fold into one ribbon only with the same label and the same pinned
    // flag: these two keep their own blocks, the pinned one first.
    let keymap_hints = build_keymap_hints(
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
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
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
    let keymap_hints = build_keymap_hints(
        vec![
            build_hint_binding(
                build_key_sequence(&[build_ctrl_chord('l')]),
                "Lock",
                false,
                false,
            ),
            build_hint_binding(
                build_key_sequence(&[KeyChord::from_parts(
                    BindingModifierFlags::ALT,
                    Key::Char('u'),
                )]),
                "Unlock",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
    );
    // The `Ctrl` group is 17 cells, the `Alt` group's ` Alt + ` header plus its
    // first ribbon is 18 more. One cell short of both, the header is skipped
    // whole and the `…` takes its place.
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 34)),
        " Ctrl +  l  Lock …"
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 35)),
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
    // Column 17 is the first cell past the ` Ctrl +  l  Lock ` group: the `…`
    // there sits on the bar background.
    assert_eq!(render_buffer[(17, 0)].symbol(), "…");
    assert_eq!(render_buffer[(17, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
    assert_eq!(render_buffer[(17, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert!(render_buffer[(17, 0)].modifier.contains(Modifier::BOLD));
}

#[test]
fn a_named_key_with_no_symbol_reads_as_its_chord_spelling() {
    let build_named_hint_binding = |named_key, action_label: &str| {
        build_hint_binding(
            build_key_sequence(&[KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Named(named_key),
            )]),
            action_label,
            false,
            false,
        )
    };
    let keymap_hints = build_keymap_hints(
        vec![
            build_named_hint_binding(NamedKey::Home, "Top"),
            build_named_hint_binding(NamedKey::End, "Bottom"),
            build_named_hint_binding(NamedKey::PageUp, "Page Up"),
            build_named_hint_binding(NamedKey::Delete, "Delete"),
            build_named_hint_binding(NamedKey::F(1), "Help"),
        ],
        &[],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
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
    );
    let buffer_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 3,
    };
    let mut render_buffer = Buffer::empty(buffer_area);
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

    let keymap_hints_without_removal =
        build_keymap_hints(create_hint_bindings(), prefix_labels, Vec::new());
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints_without_removal,
            &pending_key_sequence,
            80
        )),
        " Ctrl +  p  PANE  ▶  n  NESTED"
    );

    // `<C-p> n b` was removed. The removal sits under `<C-p>` and under
    // `<C-p> n`: both prefix labels give way to their binding counts.
    let keymap_hints_with_removal = build_keymap_hints(
        create_hint_bindings(),
        prefix_labels,
        vec![build_key_sequence(&[
            build_ctrl_chord('p'),
            build_plain_chord('n'),
            build_plain_chord('b'),
        ])],
    );
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints_with_removal,
            &pending_key_sequence,
            80
        )),
        " Ctrl +  p  +1  ▶  n  +1"
    );
}

#[test]
fn modifier_groups_read_ctrl_alt_no_modifier_ctrl_shift_shift_super_then_the_rest() {
    let build_modifier_hint_binding = |modifier_flags, key_character: char, action_label: &str| {
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
    let keymap_hints = build_keymap_hints(
        vec![
            build_modifier_hint_binding(
                BindingModifierFlags::CTRL | BindingModifierFlags::ALT,
                'f',
                "CtrlAlt",
            ),
            build_modifier_hint_binding(BindingModifierFlags::NONE, 'g', "Bare"),
            build_modifier_hint_binding(BindingModifierFlags::SUPER, 'a', "Super"),
            build_modifier_hint_binding(BindingModifierFlags::SHIFT, 'b', "Shift"),
            build_modifier_hint_binding(
                BindingModifierFlags::CTRL | BindingModifierFlags::SHIFT,
                'c',
                "CtrlShift",
            ),
            build_modifier_hint_binding(BindingModifierFlags::ALT, 'd', "Alt"),
            build_modifier_hint_binding(BindingModifierFlags::CTRL, 'e', "Ctrl"),
        ],
        &[],
        Vec::new(),
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 200)),
        concat!(
            " Ctrl +  e  Ctrl  Alt +  d  Alt  g  Bare  Ctrl+Shift +  c  CtrlShift ",
            " Shift +  b  Shift  Super +  a  Super  Ctrl+Alt +  f  CtrlAlt"
        )
    );
}

#[test]
fn only_the_opening_chord_of_a_pending_sequence_shows_a_prefix_label() {
    // `n` is a labeled top-level prefix in its own right. As the second chord
    // of the open sequence, its `NESTED` label stays off the breadcrumb: only
    // the chord that opened the sequence is labeled.
    let keymap_hints = build_keymap_hints(
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
    );
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p'), build_plain_chord('n')]);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &pending_key_sequence,
            80
        )),
        " Ctrl +  p  PANE  n  ▶  a  Deep A"
    );
}

#[test]
fn a_pending_sequence_that_is_itself_bound_lists_only_its_continuations() {
    // `<C-p>` runs an action of its own and also opens deeper bindings. With
    // `<C-p>` pending, only its continuation `n` is listed.
    let keymap_hints = build_keymap_hints(
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
    );
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
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
    let keymap_hints = build_keymap_hints(Vec::new(), &[], Vec::new());
    let render_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 1,
    };
    let mut render_buffer = Buffer::empty(render_area);
    // Pre-fill the row with stale cells.
    render_buffer.set_string(0, 0, "X".repeat(20), Style::default());
    paint_statusline(
        &keymap_hints,
        &Theme::default(),
        None,
        render_area,
        &mut render_buffer,
    );
    assert_eq!(format_rendered_row(&render_buffer), "");
    // Blank of text, but every cell still carries the bar background.
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

/// A non-default palette recolors the bar: the open-sequence breadcrumb takes
/// the theme's accent pair and a group's key block sits on the custom ramp.
#[test]
fn a_custom_theme_recolors_the_bar() {
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    let keymap_hints = build_pane_keymap_hints(false);
    let custom_theme = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ramp_end: (0x00, 0x00, 0xff),
        accent_color: Color::Rgb(0x00, 0xff, 0x00),
        accent_block_text_color: Color::Rgb(0x01, 0x02, 0x03),
        ..Theme::default()
    };
    let render_buffer = render_statusline_row(
        &keymap_hints,
        &custom_theme,
        Some(&pending_key_sequence),
        80,
    );
    // Row: " Ctrl +  p  PANE  ▶  n  New Pane  x  Close Pane". The
    // breadcrumb's `Ctrl +` is accent text; its key block is on-accent text on
    // the accent.
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
    // The area starts at column 5 and is 6 cells wide, too narrow for the
    // first ` Ctrl + ` group: the `…` takes the area's first cell.
    let keymap_hints = build_pane_keymap_hints(false);
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
    assert_eq!(render_buffer[(5, 0)].symbol(), "…");
}

#[test]
fn a_hint_wider_than_the_cell_counter_is_dropped_behind_the_overflow_marker() {
    // A ribbon of 65 536 cells reads as 65 535 cells, which never fits: the
    // row is only the `…`.
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
    );

    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 40)),
        "…"
    );
}

/// The bar's data for `lock_mode` from the shipped catalog: the built-in
/// defaults over the built-in action table, with mouse-select off and the
/// focused pane tiled.
fn build_shipped_keymap_hints(lock_mode: LockMode) -> KeymapHints {
    KeymapHintCatalog::from_registry(&ActionRegistry::new())
        .build_frame_hints(lock_mode, false, false)
}

/// The bar's data for `lock_mode` from the built-in defaults under
/// `keybindings_config`, with a user layer that binds
/// `bound_action_by_key_sequence` and removes `removed_key_sequences` in
/// `lock_mode`.
fn build_user_keymap_hints(
    keybindings_config: &KeybindingsConfig,
    lock_mode: LockMode,
    bound_action_by_key_sequence: BTreeMap<KeySequence, BoundAction>,
    removed_key_sequences: BTreeSet<KeySequence>,
) -> KeymapHints {
    let keymap_layers = build_keymap_layers(
        Some(BTreeMap::from([(
            ModeName::from_text(lock_mode.get_keymap_name()),
            ModeBindings {
                bound_action_by_key_sequence,
                removed_key_sequences,
            },
        )])),
        keybindings_config.leader,
    );
    KeymapHintCatalog::from_keymap_layers_config_and_registry(
        &keymap_layers,
        keybindings_config,
        &ActionRegistry::new(),
    )
    .build_frame_hints(lock_mode, false, false)
}

/// A binding to the `core:` action named `action_name`.
fn build_core_bound_action(action_name: &str) -> BoundAction {
    BoundAction {
        action_reference: ActionReference::from_core_action_name(action_name)
            .expect("a core action name"),
    }
}

/// An unmodified named-key chord.
fn build_plain_named_chord(named_key: NamedKey) -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(named_key))
}

#[test]
fn the_shipped_pane_prefix_row_shows_new_and_close_first_at_40_80_and_120_columns() {
    let keymap_hints = build_shipped_keymap_hints(LockMode::Normal);
    let pending_key_sequence = build_key_sequence(&[build_ctrl_chord('p')]);
    for (column_count, expected_row) in [
        (40, " Ctrl +  p  PANE  ▶  n  New Pane …"),
        (
            80,
            " Ctrl +  p  PANE  ▶  n  New Pane  x  Close Pane Tree  ←↓↑→  Focus Pane …",
        ),
        (
            120,
            concat!(
                " Ctrl +  p  PANE  ▶  n  New Pane  x  Close Pane Tree  ←↓↑→  Focus Pane ",
                " Tab  Focus Next Floating Pane  hjkl  Split Pane…"
            ),
        ),
    ] {
        assert_eq!(
            format_rendered_row(&render_pending_statusline(
                &keymap_hints,
                &pending_key_sequence,
                column_count,
            )),
            expected_row,
            "{column_count} columns"
        );
    }
}

#[test]
fn the_shipped_pane_prefix_rows_overflow_marker_takes_the_first_dropped_hints_column() {
    // At 80 columns the row ends after ` ←↓↑→  Focus Pane ` at column 71. The
    // next hint, ` Tab  Focus Next Floating Pane `, needs 31 cells, so the `…`
    // takes column 71 and every cell after it stays blank.
    let render_buffer = render_pending_statusline(
        &build_shipped_keymap_hints(LockMode::Normal),
        &build_key_sequence(&[build_ctrl_chord('p')]),
        80,
    );
    assert_eq!(render_buffer[(70, 0)].symbol(), " ");
    assert_eq!(render_buffer[(71, 0)].symbol(), "…");
    assert_eq!(render_buffer[(71, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
    assert_eq!(render_buffer[(71, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert!(render_buffer[(71, 0)].modifier.contains(Modifier::BOLD));
    for column_index in 72..80 {
        assert_eq!(
            render_buffer[(column_index, 0)].symbol(),
            " ",
            "col {column_index}"
        );
    }
}

#[test]
fn the_shipped_normal_row_at_80_columns_leads_with_lock_and_the_new_and_close_prefixes() {
    assert_eq!(
        format_rendered_row(&render_statusline(
            &build_shipped_keymap_hints(LockMode::Normal),
            80
        )),
        " Ctrl +  l  Lock  p  PANE  t  TAB  s  RESIZE  g  Mouse Select  q  Quit …"
    );
}

#[test]
fn the_shipped_normal_row_at_120_columns_puts_the_bare_tab_ahead_of_shift_tab() {
    assert_eq!(
        format_rendered_row(&render_statusline(
            &build_shipped_keymap_hints(LockMode::Normal),
            120
        )),
        concat!(
            " Ctrl +  l  Lock  p  PANE  t  TAB  s  RESIZE  g  Mouse Select  q  Quit ",
            " Alt +  f  Toggle Pane Fullscreen  Tab  Next Tab…"
        )
    );
}

#[test]
fn the_shipped_locked_row_at_80_columns_leads_with_the_pinned_unlock() {
    assert_eq!(
        format_rendered_row(&render_statusline(
            &build_shipped_keymap_hints(LockMode::Locked),
            80
        )),
        " Ctrl +  l  Unlock  g  Mouse Select  q  Quit"
    );
}

#[test]
fn the_shipped_pane_placement_row_at_80_columns_leads_with_confirm_and_cancel() {
    assert_eq!(
        format_rendered_row(&render_statusline(
            &build_shipped_keymap_hints(LockMode::PanePlacement),
            80
        )),
        " ENTER  Confirm Pane Placement  ESC  Cancel Pane Placement …"
    );
}

#[test]
fn the_shipped_floating_pane_move_row_at_80_columns_leads_with_confirm_and_cancel() {
    assert_eq!(
        format_rendered_row(&render_statusline(
            &build_shipped_keymap_hints(LockMode::FloatingPaneMove),
            80
        )),
        " ENTER  Confirm Floating Pane Move  ESC  Cancel Floating Pane Move …"
    );
}

#[test]
fn the_shipped_floating_pane_move_row_at_40_columns_shows_confirm() {
    assert_eq!(
        format_rendered_row(&render_statusline(
            &build_shipped_keymap_hints(LockMode::FloatingPaneMove),
            40
        )),
        " ENTER  Confirm Floating Pane Move …"
    );
}

#[test]
fn the_shipped_dock_undock_row_at_80_columns_leads_with_confirm_cancel_and_the_targets() {
    assert_eq!(
        format_rendered_row(&render_statusline(
            &build_shipped_keymap_hints(LockMode::DockUndock),
            80
        )),
        " ENTER  Dock Or Undock  ESC  Cancel Dock Or Undock  ←↓↑→  Select Pane Target …"
    );
}

#[test]
fn an_alt_unlock_alternative_keeps_the_pinned_unlock_first_at_40_and_80_columns() {
    // `unlock-alternative "<A-u>"`, with the locked block binding `<A-u>` to
    // `core:unlock` and removing `<C-l>`. Conflict detection applies it.
    let alternative_unlock_chord = KeyChord::from_parts(BindingModifierFlags::ALT, Key::Char('u'));
    let keybindings_config = KeybindingsConfig {
        unlock_alternative: Some(alternative_unlock_chord),
        ..KeybindingsConfig::default()
    };
    let bound_action_by_key_sequence = BTreeMap::from([(
        KeySequence::from(alternative_unlock_chord),
        build_core_bound_action("unlock"),
    )]);
    let removed_key_sequences =
        BTreeSet::from([KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)]);
    let conflict_report = detect_conflicts(
        &build_keymap_layers(
            Some(BTreeMap::from([(
                ModeName::from_text(LockMode::Locked.get_keymap_name()),
                ModeBindings {
                    bound_action_by_key_sequence: bound_action_by_key_sequence.clone(),
                    removed_key_sequences: removed_key_sequences.clone(),
                },
            )])),
            keybindings_config.leader,
        ),
        keybindings_config.leader,
        keybindings_config.unlock_alternative,
        keybindings_config.maximum_chord_depth,
        &ActionRegistry::new(),
    );
    assert_eq!(conflict_report.get_verdict(), KeymapVerdict::Apply);

    let keymap_hints = build_user_keymap_hints(
        &keybindings_config,
        LockMode::Locked,
        bound_action_by_key_sequence,
        removed_key_sequences,
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 40)),
        " Alt +  u  Unlock …"
    );
    assert_eq!(
        format_rendered_row(&render_statusline(&keymap_hints, 80)),
        " Alt +  u  Unlock  Ctrl +  g  Mouse Select  q  Quit"
    );
}

#[test]
fn a_partly_rebound_direction_family_folds_only_the_keys_that_keep_its_label() {
    // `<C-p> <Left>` rebound to `core:resize-pane-left` reads `Resize Pane`;
    // the other three arrows keep `Focus Pane` and fold without it. The user
    // binding under `<C-p>` turns its `PANE` label into the `+15` count.
    let keymap_hints = build_user_keymap_hints(
        &KeybindingsConfig::default(),
        LockMode::Normal,
        BTreeMap::from([(
            build_key_sequence(&[
                build_ctrl_chord('p'),
                build_plain_named_chord(NamedKey::Left),
            ]),
            build_core_bound_action("resize-pane-left"),
        )]),
        BTreeSet::new(),
    );
    assert_eq!(
        format_rendered_row(&render_pending_statusline(
            &keymap_hints,
            &build_key_sequence(&[build_ctrl_chord('p')]),
            120,
        )),
        concat!(
            " Ctrl +  p  +15  ▶  n  New Pane  x  Close Pane Tree  ←  Resize Pane ",
            " ↓↑→  Focus Pane  Tab  Focus Next Floating Pane …"
        )
    );
}
