//! Tests for the keybinding hint bar: idle grouping (leaf hints, labeled
//! default prefix groups, `+N` fallbacks once a user entry or removal touches
//! a group), the pending-sequence face (breadcrumb plus continuations, nested
//! groups), pinned-first ordering, whole-item truncation, the right-aligned
//! keymap-revert marker, and the blanked row for a mode with nothing to hint.

use super::*;

use std::collections::BTreeSet;
use std::sync::Arc;

use koshi_core::key::{Key, KeySequence, ModFlags, NamedKey};

use crate::snapshot::HintBinding;

/// A `Ctrl`-modified character chord.
fn ctrl(key: char) -> KeyChord {
    KeyChord::new(ModFlags::CTRL, Key::Char(key))
}

/// An unmodified character chord.
fn plain(key: char) -> KeyChord {
    KeyChord::new(ModFlags::NONE, Key::Char(key))
}

/// A sequence from chords in press order.
fn seq(chords: &[KeyChord]) -> KeySequence {
    KeySequence::new(chords[0], chords[1..].to_vec())
}

/// A hint binding with the given flags.
fn binding(sequence: KeySequence, label: &str, user_set: bool, pinned: bool) -> HintBinding {
    HintBinding {
        sequence,
        label: label.to_string(),
        user_set,
        pinned,
    }
}

/// Assemble a [`KeymapHints`] from its parts.
fn hints(
    entries: Vec<HintBinding>,
    labels: &[(KeyChord, &str)],
    removed: Vec<KeySequence>,
    reverted: bool,
) -> KeymapHints {
    KeymapHints {
        entries: Arc::new(entries),
        prefix_labels: Arc::new(
            labels
                .iter()
                .map(|(chord, label)| (*chord, (*label).to_string()))
                .collect(),
        ),
        removed: Arc::new(removed.into_iter().collect::<BTreeSet<_>>()),
        reverted,
    }
}

/// Draw the bar into a fresh one-row buffer of `width` cells.
fn draw(hints: &KeymapHints, width: u16) -> Buffer {
    draw_themed(hints, &Theme::default(), width)
}

/// Draw in `theme`'s colors with an open sequence.
fn draw_themed_pending(
    hints: &KeymapHints,
    theme: &Theme,
    pending: &KeySequence,
    width: u16,
) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width,
        height: 1,
    };
    let mut buf = Buffer::empty(area);
    draw_hint_bar(hints, theme, Some(pending), area, &mut buf);
    buf
}

/// Draw with an open sequence, which the viewer owns and hands to the bar.
fn draw_pending(hints: &KeymapHints, pending: &KeySequence, width: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width,
        height: 1,
    };
    let mut buf = Buffer::empty(area);
    draw_hint_bar(hints, &Theme::default(), Some(pending), area, &mut buf);
    buf
}

/// Paint the hint bar in `theme`'s colors, for the tests that check which
/// color a piece of the bar takes.
fn draw_themed(hints: &KeymapHints, theme: &Theme, width: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width,
        height: 1,
    };
    let mut buf = Buffer::empty(area);
    draw_hint_bar(hints, theme, None, area, &mut buf);
    buf
}

/// The buffer's single row as a string, trailing spaces trimmed.
fn row_text(buf: &Buffer) -> String {
    let row: String = (0..buf.area.width)
        .map(|x| buf[(x, 0)].symbol().to_string())
        .collect::<Vec<_>>()
        .join("");
    row.trim_end().to_string()
}

/// The default-shaped fixture: two sequences under `<C-p>` labeled `PANE`,
/// plus a single-chord `Lock` binding.
fn pane_fixture(user_close: bool) -> KeymapHints {
    hints(
        vec![
            binding(seq(&[ctrl('l')]), "Lock", false, false),
            binding(seq(&[ctrl('p'), plain('n')]), "New Pane", false, false),
            binding(
                seq(&[ctrl('p'), plain('x')]),
                "Close Pane",
                user_close,
                false,
            ),
        ],
        &[(ctrl('p'), "PANE")],
        Vec::new(),
        false,
    )
}

#[test]
fn idle_shows_leaf_hints_and_labeled_default_group() {
    let bar = pane_fixture(false);
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  l  Lock  p  PANE");
}

#[test]
fn modifier_key_and_action_ribbons_use_the_group_ramp_stop() {
    let bar = pane_fixture(false);
    let buf = draw(&bar, 80);
    // One modifier group → the ramp's purple end everywhere in it: the
    // header as text color, the key block as background, the label block as
    // the dimmed background.
    let purple = Color::Rgb(0xd0, 0xa5, 0xff);
    let purple_dim = Color::Rgb(0x72, 0x5a, 0x8c);
    assert_eq!(buf[(1, 0)].fg, purple);
    assert!(buf[(1, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(buf[(9, 0)].bg, purple);
    assert_eq!(buf[(9, 0)].fg, Color::Rgb(0x12, 0x09, 0x1f));
    assert_eq!(buf[(12, 0)].bg, purple_dim);
    assert_eq!(buf[(12, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
}

#[test]
fn human_modifier_groups_fold_same_action_keys() {
    let keymap = hints(
        vec![
            binding(
                seq(&[KeyChord::new(ModFlags::CTRL, Key::Named(NamedKey::Left))]),
                "Focus Pane",
                false,
                false,
            ),
            binding(
                seq(&[KeyChord::new(ModFlags::CTRL, Key::Named(NamedKey::Down))]),
                "Focus Pane",
                false,
                false,
            ),
            binding(
                seq(&[KeyChord::new(ModFlags::ALT, Key::Char('h'))]),
                "Focus Pane",
                false,
                false,
            ),
            binding(
                seq(&[KeyChord::new(ModFlags::ALT, Key::Char('j'))]),
                "Focus Pane",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    let bar = keymap;
    assert_eq!(
        row_text(&draw(&bar, 80)),
        " Ctrl +  ←↓  Focus Pane  Alt +  hj  Focus Pane"
    );
}

#[test]
fn bare_key_wears_the_header_style_not_a_key_block() {
    let shift_tab = KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Tab));
    let bare_tab = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Tab));
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('l')]), "Lock", false, false),
            binding(seq(&[shift_tab]), "Previous Tab", false, false),
            binding(seq(&[bare_tab]), "Next Tab", false, false),
        ],
        &[],
        Vec::new(),
        false,
    );
    let bar = keymap;
    let buf = draw(&bar, 80);
    assert_eq!(
        row_text(&buf),
        " Ctrl +  l  Lock  Shift +  Tab  Previous Tab  Tab  Next Tab"
    );
    // The Shift group's key is a block: dark text on the mid ramp stop.
    assert_eq!(buf[(27, 0)].bg, Color::Rgb(0xa7, 0xb0, 0xff));
    assert_eq!(buf[(27, 0)].fg, Color::Rgb(0x12, 0x09, 0x1f));
    // The bare Tab is its own opener: header-styled text on the bar itself —
    // the blue ramp end as foreground over the bar background, no block
    // behind it.
    assert_eq!(buf[(46, 0)].fg, Color::Rgb(0x7d, 0xbc, 0xff));
    assert_eq!(buf[(46, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert!(buf[(46, 0)].modifier.contains(Modifier::BOLD));
    // Its action label keeps the dimmed block, same as any other ribbon.
    assert_eq!(buf[(51, 0)].bg, Color::Rgb(0x44, 0x67, 0x8c));
}

#[test]
fn arrow_keys_sort_left_down_up_right_ahead_of_other_keys() {
    let arrow = |named| {
        binding(
            seq(&[KeyChord::new(ModFlags::CTRL, Key::Named(named))]),
            "Focus Pane",
            false,
            false,
        )
    };
    let keymap = hints(
        vec![
            arrow(NamedKey::Right),
            binding(seq(&[ctrl('z')]), "Focus Pane", false, false),
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
    assert_eq!(row_text(&draw(&keymap, 80)), " Ctrl +  ←↓↑→z  Focus Pane");
}

#[test]
fn named_keys_read_as_their_own_names() {
    let named = |key, label: &str| {
        binding(
            seq(&[KeyChord::new(ModFlags::NONE, Key::Named(key))]),
            label,
            false,
            false,
        )
    };
    let keymap = hints(
        vec![
            named(NamedKey::Enter, "Accept"),
            named(NamedKey::Esc, "Cancel"),
            named(NamedKey::Space, "Pick"),
            named(NamedKey::Backspace, "Undo"),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        row_text(&draw(&keymap, 80)),
        " BACKSPACE  Undo  ENTER  Accept  ESC  Cancel  SPACE  Pick"
    );
}

#[test]
fn user_entry_under_prefix_swaps_label_for_count() {
    let bar = pane_fixture(true);
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  l  Lock  p  +2");
}

#[test]
fn removal_under_prefix_swaps_label_for_count() {
    let keymap = hints(
        vec![binding(
            seq(&[ctrl('p'), plain('n')]),
            "New Pane",
            false,
            false,
        )],
        &[(ctrl('p'), "PANE")],
        vec![seq(&[ctrl('p'), plain('x')])],
        false,
    );
    let bar = keymap;
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  p  +1");
}

#[test]
fn unlabeled_group_shows_count() {
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('t'), plain('n')]), "New Tab", false, false),
            binding(seq(&[ctrl('t'), plain('x')]), "Close Tab", false, false),
        ],
        &[],
        Vec::new(),
        false,
    );
    let bar = keymap;
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  t  +2");
}

#[test]
fn pending_prefix_shows_breadcrumb_and_continuations() {
    let pending = seq(&[ctrl('p')]);
    let bar = pane_fixture(false);
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 80)),
        " Ctrl +  p  PANE  ▶  n  New Pane  x  Close Pane"
    );
}

#[test]
fn pending_prefix_with_no_continuations_shows_bare_breadcrumb_and_no_groups() {
    // The user pressed a chord that isn't a prefix of anything bound: no
    // matching entries means no label and no continuation groups — just the
    // breadcrumb and arrow, with no panic on the now-empty group list.
    let pending = seq(&[ctrl('z')]);
    let bar = pane_fixture(false);
    assert_eq!(row_text(&draw_pending(&bar, &pending, 80)), " Ctrl +  z  ▶");
}

#[test]
fn customized_pending_prefix_uses_count_not_shipped_label() {
    let pending = seq(&[ctrl('p')]);
    let bar = pane_fixture(true);
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 80)),
        " Ctrl +  p  +2  ▶  n  New Pane  x  Close Pane"
    );
}

#[test]
fn pending_prefix_without_label_shows_derived_count() {
    let keymap = hints(
        vec![binding(
            seq(&[ctrl('t'), plain('n')]),
            "New Tab",
            false,
            false,
        )],
        &[],
        Vec::new(),
        false,
    );
    let pending = seq(&[ctrl('t')]);
    let bar = keymap;
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 80)),
        " Ctrl +  t  +1  ▶  n  New Tab"
    );
}

#[test]
fn nested_group_inside_pending_shows_count() {
    let keymap = hints(
        vec![
            binding(
                seq(&[ctrl('p'), plain('n'), plain('a')]),
                "Deep A",
                false,
                false,
            ),
            binding(
                seq(&[ctrl('p'), plain('n'), plain('b')]),
                "Deep B",
                false,
                false,
            ),
        ],
        &[(ctrl('p'), "PANE")],
        Vec::new(),
        false,
    );
    let pending = seq(&[ctrl('p')]);
    let bar = keymap;
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 80)),
        " Ctrl +  p  PANE  ▶  n  +2"
    );
}

#[test]
fn chord_bound_and_extended_shows_action_with_count() {
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('p')]), "Pane Menu", false, false),
            binding(seq(&[ctrl('p'), plain('n')]), "New Pane", false, false),
        ],
        &[(ctrl('p'), "PANE")],
        Vec::new(),
        false,
    );
    let bar = keymap;
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  p  Pane Menu +1");
}

#[test]
fn pinned_hint_sorts_first_and_survives_truncation() {
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('a')]), "Aardvark", false, false),
            binding(seq(&[ctrl('g')]), "Unlock", false, true),
        ],
        &[],
        Vec::new(),
        false,
    );
    let bar = keymap;
    // Wide: pinned first despite `<C-a>` sorting lower.
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  g  Unlock  a  Aardvark");
    // Narrow: only the pinned hint fits; the dropped one leaves a `…`.
    assert_eq!(row_text(&draw(&bar, 19)), " Ctrl +  g  Unlock…");
}

#[test]
fn truncation_drops_whole_trailing_hints() {
    let bar = pane_fixture(false);
    // Shared `Ctrl +` header plus the first ribbon is 17 cells; the second
    // ribbon needs 9 more, so below 26 it is dropped whole behind a `…`.
    assert_eq!(row_text(&draw(&bar, 25)), " Ctrl +  l  Lock …");
    assert_eq!(row_text(&draw(&bar, 26)), " Ctrl +  l  Lock  p  PANE");
}

#[test]
fn an_overflow_marker_with_no_cell_left_takes_the_last_one() {
    let bar = pane_fixture(false);
    // The `Ctrl +` header plus the first ribbon fill all 17 cells exactly, so
    // the `…` standing for the dropped second ribbon has no cell of its own and
    // overwrites the last cell of the ribbon before it.
    assert_eq!(row_text(&draw(&bar, 17)), " Ctrl +  l  Lock…");
}

#[test]
fn revert_marker_holds_right_edge_and_hints_stop_short() {
    let keymap = KeymapHints {
        reverted: true,
        ..pane_fixture(false)
    };
    let bar = keymap;
    let buf = draw(&bar, 30);
    let row = row_text(&buf);
    assert_eq!(row, " Ctrl +  l  Lock …      keys!");
    // Marker text holds the right edge, with one background-padding cell.
    assert_eq!(buf[(28, 0)].symbol(), "!");
}

#[test]
fn empty_mode_blanks_the_row() {
    let bar = hints(Vec::new(), &[], Vec::new(), false);
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 1,
    };
    let mut buf = Buffer::empty(area);
    // Pre-fill the row: the bar owns it, so stale cells must be cleared.
    buf.set_string(0, 0, "X".repeat(20), Style::default());
    draw_hint_bar(&bar, &Theme::default(), None, area, &mut buf);
    assert_eq!(row_text(&buf), "");
    // Blank of text, but not of color: the row still carries the bar
    // background, so an empty mode reads as a bar rather than a hole.
    for x in 0..20 {
        assert_eq!(buf[(x, 0)].bg, Color::Rgb(0x00, 0x00, 0x00), "col {x}");
    }
}

#[test]
fn zero_size_area_draws_nothing() {
    let bar = pane_fixture(false);
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };
    let mut buf = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 10,
        height: 1,
    });
    draw_hint_bar(&bar, &Theme::default(), None, area, &mut buf);
    assert_eq!(row_text(&buf), "");
}

/// A non-default palette recolors the bar: the pending breadcrumb takes the
/// theme's accent pair and a group's key block sits on the custom ramp.
#[test]
fn a_custom_theme_recolors_the_bar() {
    let pending = seq(&[ctrl('p')]);
    let bar = pane_fixture(false);
    let theme = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ramp_end: (0x00, 0x00, 0xff),
        accent: Color::Rgb(0x00, 0xff, 0x00),
        on_accent: Color::Rgb(0x01, 0x02, 0x03),
        ..Theme::default()
    };
    let buf = draw_themed_pending(&bar, &theme, &pending, 80);
    // Row: " Ctrl +  p  PANE  ▶  n  New Pane …". The breadcrumb's `Ctrl +`
    // is accent text; its key block is on-accent text on the accent.
    assert_eq!(buf[(1, 0)].fg, Color::Rgb(0x00, 0xff, 0x00));
    assert_eq!(buf[(9, 0)].fg, Color::Rgb(0x01, 0x02, 0x03));
    assert_eq!(buf[(9, 0)].bg, Color::Rgb(0x00, 0xff, 0x00));
    // The modifier-less continuation key wears the group's header style: the
    // custom ramp's start stop as its text color.
    let n_x = (0..80)
        .find(|&x| buf[(x, 0)].symbol() == "n")
        .expect("continuation key drawn");
    assert_eq!(buf[(n_x, 0)].fg, Color::Rgb(0xff, 0x00, 0x00));
}
